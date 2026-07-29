//! Crash-safe publication and append-only record storage.

#![forbid(unsafe_code)]

use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

#[cfg(unix)]
use std::fs::File;

use anyhow::{Context, Result, bail};
use serde::{Serialize, de::DeserializeOwned};

/// Publishes immutable content without ever exposing a partially written final
/// path. A concurrent publisher of identical content is accepted.
pub fn publish_immutable(path: &Path, content: &[u8]) -> Result<()> {
    let parent = parent_directory(path)?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create storage directory {}", parent.display()))?;
    match fs::read(path) {
        Ok(existing) if existing == content => return Ok(()),
        Ok(_) => bail!("immutable file collision at {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    }
    let mut staged = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to stage immutable file {}", path.display()))?;
    staged
        .write_all(content)
        .with_context(|| format!("failed to stage immutable file {}", path.display()))?;
    staged
        .as_file()
        .sync_all()
        .with_context(|| format!("failed to flush immutable file {}", path.display()))?;

    match staged.persist_noclobber(path) {
        Ok(_) => sync_directory(parent),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing =
                fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
            if existing == content {
                Ok(())
            } else {
                bail!("immutable file collision at {}", path.display())
            }
        }
        Err(error) => Err(error.error)
            .with_context(|| format!("failed to publish immutable file {}", path.display())),
    }
}

/// Atomically replaces a mutable file after its complete contents reach stable
/// storage. Readers see either the previous version or the replacement.
pub fn replace_file(path: &Path, content: &[u8]) -> Result<()> {
    let parent = parent_directory(path)?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create storage directory {}", parent.display()))?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to stage replacement file {}", path.display()))?;
    staged
        .write_all(content)
        .with_context(|| format!("failed to stage replacement file {}", path.display()))?;
    staged
        .as_file()
        .sync_all()
        .with_context(|| format!("failed to flush replacement file {}", path.display()))?;
    staged
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to replace file {}", path.display()))?;
    sync_directory(parent)
}

/// Appends one committed JSON record. An interrupted trailing record is removed
/// under the journal lock before the new record is written.
pub fn append_json_line<Value: Serialize>(path: &Path, value: &Value) -> Result<()> {
    let parent = parent_directory(path)?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create journal directory {}", parent.display()))?;
    let journal_existed = path.exists();
    let mut encoded = serde_json::to_vec(value).context("failed to encode journal record")?;
    encoded.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open journal {}", path.display()))?;
    file.lock()
        .with_context(|| format!("failed to lock journal {}", path.display()))?;
    let mut content = Vec::new();
    file.read_to_end(&mut content)
        .with_context(|| format!("failed to inspect journal {}", path.display()))?;
    let committed_len = committed_prefix_len(&content);
    if committed_len != content.len() {
        file.set_len(committed_len as u64)
            .with_context(|| format!("failed to recover journal {}", path.display()))?;
    }
    file.seek(SeekFrom::End(0))
        .with_context(|| format!("failed to seek journal {}", path.display()))?;
    file.write_all(&encoded)
        .with_context(|| format!("failed to append journal {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to flush journal {}", path.display()))?;
    if journal_existed {
        Ok(())
    } else {
        sync_directory(parent)
    }
}

/// Reads only newline-committed JSON records. A partial trailing record is
/// ignored so the owning operation can resume and replace it on append.
pub fn read_json_lines<Value: DeserializeOwned>(path: &Path) -> Result<Vec<Value>> {
    let Some(content) = read_committed_journal(path)? else {
        return Ok(Vec::new());
    };
    content
        .split(|byte| *byte == b'\n')
        .enumerate()
        .filter(|(_index, line)| !line.iter().all(u8::is_ascii_whitespace))
        .map(|(index, line)| {
            serde_json::from_slice(line).with_context(|| {
                format!("invalid JSON in {} at line {}", path.display(), index + 1)
            })
        })
        .collect()
}

/// Reads the most recent newline-committed JSON record without requiring older
/// records to use the current domain schema.
pub fn read_last_json_line<Value: DeserializeOwned>(path: &Path) -> Result<Option<Value>> {
    let Some(content) = read_committed_journal(path)? else {
        return Ok(None);
    };
    let Some(line) = content
        .rsplit(|byte| *byte == b'\n')
        .find(|line| !line.iter().all(u8::is_ascii_whitespace))
    else {
        return Ok(None);
    };
    serde_json::from_slice(line)
        .with_context(|| format!("invalid final JSON record in {}", path.display()))
        .map(Some)
}

fn read_committed_journal(path: &Path) -> Result<Option<Vec<u8>>> {
    let mut file = match OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to open journal {}", path.display()));
        }
    };
    file.lock()
        .with_context(|| format!("failed to lock journal {}", path.display()))?;
    let mut content = Vec::new();
    file.read_to_end(&mut content)
        .with_context(|| format!("failed to read journal {}", path.display()))?;
    content.truncate(committed_prefix_len(&content));
    Ok(Some(content))
}

fn committed_prefix_len(content: &[u8]) -> usize {
    if content.last() == Some(&b'\n') {
        content.len()
    } else {
        content
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1)
    }
}

fn parent_directory(path: &Path) -> Result<&Path> {
    let parent = path
        .parent()
        .with_context(|| format!("storage path has no parent: {}", path.display()))?;
    Ok(if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    })
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("failed to open storage directory {}", path.display()))?
        .sync_all()
        .with_context(|| format!("failed to flush storage directory {}", path.display()))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde::{Deserialize, Serialize};
    use tempfile::tempdir;

    use super::*;

    #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
    struct Record {
        value: u32,
    }

    #[test]
    fn immutable_publication_accepts_only_an_identical_winner() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("object");
        publish_immutable(&path, b"complete").unwrap();
        publish_immutable(&path, b"complete").unwrap();
        assert!(publish_immutable(&path, b"different").is_err());
        assert_eq!(fs::read(path).unwrap(), b"complete");
    }

    #[test]
    fn journal_recovers_an_interrupted_trailing_record() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("records.jsonl");
        append_json_line(&path, &Record { value: 1 }).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(br#"{"value":"#)
            .unwrap();

        assert_eq!(
            read_json_lines::<Record>(&path).unwrap(),
            [Record { value: 1 }]
        );
        append_json_line(&path, &Record { value: 2 }).unwrap();
        assert_eq!(
            read_json_lines::<Record>(&path).unwrap(),
            [Record { value: 1 }, Record { value: 2 }]
        );
        assert!(fs::read(&path).unwrap().ends_with(b"\n"));
    }

    #[test]
    fn latest_record_does_not_require_legacy_records_to_deserialize() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("records.jsonl");
        fs::write(&path, b"{\"legacy\":true}\n{\"value\":2}\n").unwrap();
        assert_eq!(
            read_last_json_line::<Record>(&path).unwrap(),
            Some(Record { value: 2 })
        );
    }

    #[test]
    fn replacement_never_leaves_staged_bytes_at_the_final_path() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("lock.json");
        replace_file(&path, b"first").unwrap();
        replace_file(&path, b"second").unwrap();
        assert_eq!(fs::read(path).unwrap(), b"second");
    }
}
