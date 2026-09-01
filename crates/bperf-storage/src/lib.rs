//! Crash-safe database and external-payload storage.

#![forbid(unsafe_code)]

use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::Path,
};

#[cfg(unix)]
use std::fs::File;

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;

pub mod database;

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

    publish_staged_immutable(path, staged)
}

/// Publishes a complete tempfile staged in the final path's directory. A
/// concurrent publisher of identical content is accepted.
pub fn publish_staged_immutable(path: &Path, staged: tempfile::NamedTempFile) -> Result<()> {
    let parent = parent_directory(path)?;
    if staged.path().parent() != Some(parent) {
        bail!(
            "immutable file {} was staged outside its storage directory",
            path.display()
        );
    }
    staged
        .as_file()
        .sync_all()
        .with_context(|| format!("failed to flush immutable file {}", path.display()))?;

    match staged.persist_noclobber(path) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            let staged = error.file.reopen().with_context(|| {
                format!("failed to reopen staged immutable file {}", path.display())
            })?;
            let existing = fs::File::open(path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            if !files_equal(staged, existing)
                .with_context(|| format!("failed to compare immutable file {}", path.display()))?
            {
                bail!("immutable file collision at {}", path.display())
            }
        }
        Err(error) => {
            return Err(error.error)
                .with_context(|| format!("failed to publish immutable file {}", path.display()));
        }
    }
    // An identical winner may have crashed before flushing the directory entry.
    sync_directory(parent)
}

fn files_equal(first: fs::File, second: fs::File) -> std::io::Result<bool> {
    let mut first = BufReader::new(first);
    let mut second = BufReader::new(second);
    loop {
        let first_buffer = first.fill_buf()?;
        let second_buffer = second.fill_buf()?;
        if first_buffer.is_empty() || second_buffer.is_empty() {
            return Ok(first_buffer.is_empty() && second_buffer.is_empty());
        }
        let compared = first_buffer.len().min(second_buffer.len());
        if first_buffer[..compared] != second_buffer[..compared] {
            return Ok(false);
        }
        first.consume(compared);
        second.consume(compared);
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

/// Reads only newline-committed JSON records. A partial trailing record is
/// ignored so an interrupted producer cannot expose a half-written value.
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

    use serde::Deserialize;
    use tempfile::tempdir;

    use super::*;

    #[derive(Debug, Deserialize, Eq, PartialEq)]
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
    fn json_line_reader_ignores_an_interrupted_trailing_record() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("records.jsonl");
        fs::write(&path, b"{\"value\":1}\n{\"value\":").unwrap();

        assert_eq!(
            read_json_lines::<Record>(&path).unwrap(),
            [Record { value: 1 }]
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
