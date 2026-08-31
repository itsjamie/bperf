use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Serialize, de::DeserializeOwned};

const DATABASE_NAME: &str = "bperf.sqlite3";
const APPLICATION_ID: i64 = 0x4250_4631;
const STORAGE_SCHEMA_VERSION: i64 = 1;

/// Canonical structured storage for one bperf data directory.
///
/// Record schemas and logical keys remain domain concepts. Connections use one
/// durability policy and ordered record contract; large browser and source
/// payloads remain ordinary files.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Database {
    root: PathBuf,
    path: PathBuf,
}

impl Database {
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)
            .with_context(|| format!("failed to create bperf data directory {}", root.display()))?;
        let root = fs::canonicalize(root).with_context(|| {
            format!("failed to resolve bperf data directory {}", root.display())
        })?;
        let database = Self {
            path: root.join(DATABASE_NAME),
            root,
        };
        let connection = database.connect_write()?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;
                 PRAGMA wal_autocheckpoint = 1000;",
            )
            .context("failed to configure bperf database durability")?;
        initialize_schema(&connection)?;
        Ok(database)
    }

    /// Resolves a conventional collection such as `measurements` or `lineages`
    /// to its enclosing data directory. Any other path is itself a data root.
    pub fn for_collection(collection_root: &Path, conventional_name: &str) -> Result<Self> {
        let data_root = if collection_root.file_name().and_then(|value| value.to_str())
            == Some(conventional_name)
        {
            collection_root.parent().with_context(|| {
                format!(
                    "bperf {conventional_name} collection has no data directory: {}",
                    collection_root.display()
                )
            })?
        } else {
            collection_root
        };
        Self::open(data_root)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn same_store(&self, other: &Self) -> bool {
        self.path == other.path
    }

    pub fn reader(&self) -> Result<DatabaseReader> {
        Ok(DatabaseReader {
            connection: self.connect_read()?,
        })
    }

    pub fn read_document<Value: DeserializeOwned>(
        &self,
        namespace: &str,
        key: &str,
    ) -> Result<Option<Value>> {
        self.reader()?.read_document(namespace, key)
    }

    pub fn read_document_bytes(&self, namespace: &str, key: &str) -> Result<Option<Vec<u8>>> {
        self.reader()?.read_document_bytes(namespace, key)
    }

    pub fn publish_document<Value: Serialize>(
        &self,
        namespace: &str,
        key: &str,
        value: &Value,
    ) -> Result<()> {
        let payload = serde_json::to_vec(value)
            .with_context(|| format!("failed to encode {namespace} document {key:?}"))?;
        self.publish_document_bytes(namespace, key, &payload)
    }

    pub fn publish_document_bytes(&self, namespace: &str, key: &str, payload: &[u8]) -> Result<()> {
        self.write(|transaction| transaction.publish_document(namespace, key, payload))
    }

    pub fn replace_document<Value: Serialize>(
        &self,
        namespace: &str,
        key: &str,
        value: &Value,
    ) -> Result<()> {
        let payload = serde_json::to_vec(value)
            .with_context(|| format!("failed to encode {namespace} document {key:?}"))?;
        self.replace_document_bytes(namespace, key, &payload)
    }

    pub fn replace_document_bytes(&self, namespace: &str, key: &str, payload: &[u8]) -> Result<()> {
        self.write(|transaction| transaction.replace_document(namespace, key, payload))
    }

    pub fn append_event<Value: Serialize>(
        &self,
        namespace: &str,
        stream: &str,
        value: &Value,
    ) -> Result<u64> {
        let payload = serde_json::to_vec(value)
            .with_context(|| format!("failed to encode {namespace} event for {stream:?}"))?;
        self.write(|transaction| transaction.append_event(namespace, stream, &payload))
    }

    /// Appends only when the stream still has the number of events observed by
    /// the caller. This turns a read/derive/append sequence into a safe
    /// optimistic update instead of allowing a stale derivation to corrupt an
    /// ordered domain journal.
    pub fn append_event_if_unchanged<Value: Serialize>(
        &self,
        namespace: &str,
        stream: &str,
        observed_events: usize,
        value: &Value,
    ) -> Result<u64> {
        let payload = serde_json::to_vec(value)
            .with_context(|| format!("failed to encode {namespace} event for {stream:?}"))?;
        self.write(|transaction| {
            transaction.append_event_if_unchanged(namespace, stream, observed_events, &payload)
        })
    }

    pub fn read_events<Value: DeserializeOwned>(
        &self,
        namespace: &str,
        stream: &str,
    ) -> Result<Vec<Value>> {
        self.reader()?.read_events(namespace, stream)
    }

    pub fn read_last_event<Value: DeserializeOwned>(
        &self,
        namespace: &str,
        stream: &str,
    ) -> Result<Option<Value>> {
        self.reader()?.read_last_event(namespace, stream)
    }

    pub fn streams(&self, namespace: &str) -> Result<Vec<String>> {
        self.reader()?.streams(namespace)
    }

    pub fn has_events(&self, namespace: &str, stream: &str) -> Result<bool> {
        self.reader()?.has_events(namespace, stream)
    }

    pub fn write<Value>(
        &self,
        operation: impl FnOnce(&mut WriteTransaction<'_>) -> Result<Value>,
    ) -> Result<Value> {
        let mut connection = self.connect_write()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("failed to begin bperf database transaction")?;
        let mut transaction = WriteTransaction { transaction };
        let value = operation(&mut transaction)?;
        transaction
            .transaction
            .commit()
            .context("failed to commit bperf database transaction")?;
        Ok(value)
    }

    fn connect_read(&self) -> Result<Connection> {
        let connection = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("failed to open bperf database {}", self.path.display()))?;
        connection
            .busy_timeout(Duration::from_millis(250))
            .context("failed to configure bperf database read timeout")?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA query_only = ON;",
            )
            .context("failed to configure bperf database reader")?;
        Ok(connection)
    }

    fn connect_write(&self) -> Result<Connection> {
        let connection = Connection::open(&self.path)
            .with_context(|| format!("failed to open bperf database {}", self.path.display()))?;
        connection
            .busy_timeout(Duration::from_secs(30))
            .context("failed to configure bperf database lock timeout")?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA synchronous = FULL;
                 PRAGMA wal_autocheckpoint = 1000;",
            )
            .context("failed to configure bperf database durability")?;
        Ok(connection)
    }
}

/// Reusable query-only connection without a long-lived read transaction.
pub struct DatabaseReader {
    connection: Connection,
}

impl DatabaseReader {
    pub fn read_document<Value: DeserializeOwned>(
        &self,
        namespace: &str,
        key: &str,
    ) -> Result<Option<Value>> {
        let Some(payload) = self.read_document_bytes(namespace, key)? else {
            return Ok(None);
        };
        serde_json::from_slice(&payload)
            .with_context(|| format!("invalid {namespace} document {key:?}"))
            .map(Some)
    }

    pub fn read_document_bytes(&self, namespace: &str, key: &str) -> Result<Option<Vec<u8>>> {
        validate_storage_key("namespace", namespace)?;
        validate_storage_key("document key", key)?;
        self.connection
            .query_row(
                "SELECT payload FROM storage_documents WHERE namespace = ?1 AND key = ?2",
                params![namespace, key],
                |row| row.get(0),
            )
            .optional()
            .with_context(|| format!("failed to read {namespace} document {key:?}"))
    }

    pub fn read_events<Value: DeserializeOwned>(
        &self,
        namespace: &str,
        stream: &str,
    ) -> Result<Vec<Value>> {
        validate_storage_key("namespace", namespace)?;
        validate_storage_key("event stream", stream)?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT payload FROM storage_events \
                 WHERE namespace = ?1 AND stream = ?2 ORDER BY sequence",
            )
            .with_context(|| format!("failed to prepare {namespace} event query"))?;
        let payloads = statement
            .query_map(params![namespace, stream], |row| row.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .with_context(|| format!("failed to read {namespace} events for {stream:?}"))?;
        payloads
            .into_iter()
            .enumerate()
            .map(|(index, payload)| {
                serde_json::from_slice(&payload).with_context(|| {
                    format!("invalid {namespace} event {} for {stream:?}", index + 1)
                })
            })
            .collect()
    }

    pub fn read_last_event<Value: DeserializeOwned>(
        &self,
        namespace: &str,
        stream: &str,
    ) -> Result<Option<Value>> {
        validate_storage_key("namespace", namespace)?;
        validate_storage_key("event stream", stream)?;
        let payload: Option<Vec<u8>> = self
            .connection
            .query_row(
                "SELECT payload FROM storage_events \
                 WHERE namespace = ?1 AND stream = ?2 ORDER BY sequence DESC LIMIT 1",
                params![namespace, stream],
                |row| row.get(0),
            )
            .optional()
            .with_context(|| format!("failed to read final {namespace} event for {stream:?}"))?;
        payload
            .map(|payload| {
                serde_json::from_slice(&payload)
                    .with_context(|| format!("invalid final {namespace} event for {stream:?}"))
            })
            .transpose()
    }

    pub fn streams(&self, namespace: &str) -> Result<Vec<String>> {
        validate_storage_key("namespace", namespace)?;
        let mut statement = self.connection.prepare(
            "SELECT DISTINCT stream FROM storage_events \
             WHERE namespace = ?1 ORDER BY stream",
        )?;
        statement
            .query_map([namespace], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .with_context(|| format!("failed to list {namespace} event streams"))
    }

    pub fn has_events(&self, namespace: &str, stream: &str) -> Result<bool> {
        validate_storage_key("namespace", namespace)?;
        validate_storage_key("event stream", stream)?;
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM storage_events \
                 WHERE namespace = ?1 AND stream = ?2)",
                params![namespace, stream],
                |row| row.get(0),
            )
            .with_context(|| format!("failed to inspect {namespace} events for {stream:?}"))
    }
}

pub struct WriteTransaction<'connection> {
    transaction: Transaction<'connection>,
}

impl WriteTransaction<'_> {
    pub fn read_events<Value: DeserializeOwned>(
        &self,
        namespace: &str,
        stream: &str,
    ) -> Result<Vec<Value>> {
        validate_storage_key("namespace", namespace)?;
        validate_storage_key("event stream", stream)?;
        let mut statement = self.transaction.prepare(
            "SELECT payload FROM storage_events \
             WHERE namespace = ?1 AND stream = ?2 ORDER BY sequence",
        )?;
        let payloads = statement
            .query_map(params![namespace, stream], |row| row.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        payloads
            .into_iter()
            .enumerate()
            .map(|(index, payload)| {
                serde_json::from_slice(&payload).with_context(|| {
                    format!("invalid {namespace} event {} for {stream:?}", index + 1)
                })
            })
            .collect()
    }

    pub fn read_last_event<Value: DeserializeOwned>(
        &self,
        namespace: &str,
        stream: &str,
    ) -> Result<Option<Value>> {
        validate_storage_key("namespace", namespace)?;
        validate_storage_key("event stream", stream)?;
        let payload: Option<Vec<u8>> = self
            .transaction
            .query_row(
                "SELECT payload FROM storage_events \
                 WHERE namespace = ?1 AND stream = ?2 ORDER BY sequence DESC LIMIT 1",
                params![namespace, stream],
                |row| row.get(0),
            )
            .optional()?;
        payload
            .map(|payload| {
                serde_json::from_slice(&payload)
                    .with_context(|| format!("invalid final {namespace} event for {stream:?}"))
            })
            .transpose()
    }

    pub fn has_events(&self, namespace: &str, stream: &str) -> Result<bool> {
        validate_storage_key("namespace", namespace)?;
        validate_storage_key("event stream", stream)?;
        self.transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM storage_events \
                 WHERE namespace = ?1 AND stream = ?2)",
                params![namespace, stream],
                |row| row.get(0),
            )
            .with_context(|| format!("failed to inspect {namespace} events for {stream:?}"))
    }

    pub fn publish_document(&mut self, namespace: &str, key: &str, payload: &[u8]) -> Result<()> {
        validate_storage_key("namespace", namespace)?;
        validate_storage_key("document key", key)?;
        let existing: Option<(bool, Vec<u8>)> = self
            .transaction
            .query_row(
                "SELECT immutable, payload FROM storage_documents \
                 WHERE namespace = ?1 AND key = ?2",
                params![namespace, key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match existing {
            Some((_immutable, existing)) if existing == payload => Ok(()),
            Some(_) => bail!("immutable {namespace} document collision at {key:?}"),
            None => {
                self.transaction.execute(
                    "INSERT INTO storage_documents(namespace, key, immutable, payload) \
                     VALUES (?1, ?2, 1, ?3)",
                    params![namespace, key, payload],
                )?;
                Ok(())
            }
        }
    }

    pub fn replace_document(&mut self, namespace: &str, key: &str, payload: &[u8]) -> Result<()> {
        validate_storage_key("namespace", namespace)?;
        validate_storage_key("document key", key)?;
        let immutable: Option<bool> = self
            .transaction
            .query_row(
                "SELECT immutable FROM storage_documents WHERE namespace = ?1 AND key = ?2",
                params![namespace, key],
                |row| row.get(0),
            )
            .optional()?;
        if immutable == Some(true) {
            bail!("cannot replace immutable {namespace} document {key:?}");
        }
        self.transaction.execute(
            "INSERT INTO storage_documents(namespace, key, immutable, payload) \
             VALUES (?1, ?2, 0, ?3) \
             ON CONFLICT(namespace, key) DO UPDATE SET payload = excluded.payload",
            params![namespace, key, payload],
        )?;
        Ok(())
    }

    pub fn append_event(&mut self, namespace: &str, stream: &str, payload: &[u8]) -> Result<u64> {
        validate_storage_key("namespace", namespace)?;
        validate_storage_key("event stream", stream)?;
        let next: i64 = self.transaction.query_row(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM storage_events \
             WHERE namespace = ?1 AND stream = ?2",
            params![namespace, stream],
            |row| row.get(0),
        )?;
        self.transaction.execute(
            "INSERT INTO storage_events(namespace, stream, sequence, payload) \
             VALUES (?1, ?2, ?3, ?4)",
            params![namespace, stream, next, payload],
        )?;
        u64::try_from(next).context("event sequence does not fit in u64")
    }

    pub fn append_event_if_unchanged(
        &mut self,
        namespace: &str,
        stream: &str,
        observed_events: usize,
        payload: &[u8],
    ) -> Result<u64> {
        validate_storage_key("namespace", namespace)?;
        validate_storage_key("event stream", stream)?;
        let current: i64 = self.transaction.query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM storage_events \
             WHERE namespace = ?1 AND stream = ?2",
            params![namespace, stream],
            |row| row.get(0),
        )?;
        let observed = i64::try_from(observed_events).context("event count does not fit in i64")?;
        if current != observed {
            bail!("{namespace} event stream {stream:?} changed after it was read");
        }
        self.append_event(namespace, stream, payload)
    }
}

fn initialize_schema(connection: &Connection) -> Result<()> {
    let application_id: i64 =
        connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    if application_id != 0 && application_id != APPLICATION_ID {
        bail!("database is not a bperf data store");
    }
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == STORAGE_SCHEMA_VERSION && application_id == APPLICATION_ID {
        return Ok(());
    }
    if version != 0 {
        bail!("unsupported bperf storage schema {version}; expected {STORAGE_SCHEMA_VERSION}");
    }
    connection
        .execute_batch(&format!(
            "BEGIN IMMEDIATE;
             PRAGMA application_id = {APPLICATION_ID};
             CREATE TABLE IF NOT EXISTS storage_documents (
                 namespace TEXT NOT NULL,
                 key TEXT NOT NULL,
                 immutable INTEGER NOT NULL CHECK (immutable IN (0, 1)),
                 payload BLOB NOT NULL,
                 PRIMARY KEY(namespace, key)
             ) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS storage_events (
                 namespace TEXT NOT NULL,
                 stream TEXT NOT NULL,
                 sequence INTEGER NOT NULL CHECK (sequence > 0),
                 payload BLOB NOT NULL,
                 PRIMARY KEY(namespace, stream, sequence)
             ) WITHOUT ROWID;
             PRAGMA user_version = {STORAGE_SCHEMA_VERSION};
             COMMIT;"
        ))
        .context("failed to initialize bperf database schema")?;
    Ok(())
}

fn validate_storage_key(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() || value.contains('\0') {
        bail!("{label} must be non-empty and contain no NUL bytes");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};
    use tempfile::tempdir;

    use super::*;

    #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
    struct Record {
        value: u32,
    }

    #[test]
    fn immutable_documents_and_mutable_documents_have_distinct_contracts() {
        let directory = tempdir().unwrap();
        let database = Database::open(directory.path()).unwrap();

        database
            .publish_document("test", "fixed", &Record { value: 1 })
            .unwrap();
        database
            .publish_document("test", "fixed", &Record { value: 1 })
            .unwrap();
        assert!(
            database
                .publish_document("test", "fixed", &Record { value: 2 })
                .is_err()
        );

        database
            .replace_document("test", "current", &Record { value: 1 })
            .unwrap();
        database
            .replace_document("test", "current", &Record { value: 2 })
            .unwrap();
        assert_eq!(
            database.read_document::<Record>("test", "current").unwrap(),
            Some(Record { value: 2 })
        );
    }

    #[test]
    fn ordered_events_commit_atomically() {
        let directory = tempdir().unwrap();
        let database = Database::open(directory.path()).unwrap();
        database
            .write(|transaction| -> Result<()> {
                transaction.append_event(
                    "test",
                    "history",
                    &serde_json::to_vec(&Record { value: 1 })?,
                )?;
                transaction.append_event(
                    "test",
                    "history",
                    &serde_json::to_vec(&Record { value: 2 })?,
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            database.read_events::<Record>("test", "history").unwrap(),
            [Record { value: 1 }, Record { value: 2 }]
        );
        assert_eq!(
            database
                .read_last_event::<Record>("test", "history")
                .unwrap(),
            Some(Record { value: 2 })
        );
    }

    #[test]
    fn conditional_append_rejects_a_stale_stream_snapshot() {
        let directory = tempdir().unwrap();
        let database = Database::open(directory.path()).unwrap();
        database
            .append_event_if_unchanged("test", "history", 0, &Record { value: 1 })
            .unwrap();
        database
            .append_event_if_unchanged("test", "history", 1, &Record { value: 2 })
            .unwrap();

        let error = database
            .append_event_if_unchanged("test", "history", 1, &Record { value: 3 })
            .unwrap_err();
        assert!(error.to_string().contains("changed after it was read"));
        assert_eq!(
            database.read_events::<Record>("test", "history").unwrap(),
            [Record { value: 1 }, Record { value: 2 }]
        );
    }

    #[test]
    fn failed_cross_domain_write_leaves_no_partial_events() {
        let directory = tempdir().unwrap();
        let database = Database::open(directory.path()).unwrap();
        let error = database
            .write(|transaction| -> Result<()> {
                transaction.append_event(
                    "baseline",
                    "parser",
                    &serde_json::to_vec(&Record { value: 1 })?,
                )?;
                transaction.append_event(
                    "lineage",
                    "parser",
                    &serde_json::to_vec(&Record { value: 2 })?,
                )?;
                bail!("simulated acceptance failure")
            })
            .unwrap_err();
        assert!(error.to_string().contains("simulated acceptance failure"));
        assert!(!database.has_events("baseline", "parser").unwrap());
        assert!(!database.has_events("lineage", "parser").unwrap());
    }

    #[test]
    fn collection_roots_share_their_parent_database() {
        let directory = tempdir().unwrap();
        let measurements = directory.path().join("measurements");
        let lineages = directory.path().join("lineages");
        fs::create_dir_all(&measurements).unwrap();
        fs::create_dir_all(&lineages).unwrap();
        let measurement_database = Database::for_collection(&measurements, "measurements").unwrap();
        let lineage_database = Database::for_collection(&lineages, "lineages").unwrap();
        assert!(measurement_database.same_store(&lineage_database));
        assert_eq!(
            measurement_database.path(),
            fs::canonicalize(directory.path())
                .unwrap()
                .join(DATABASE_NAME)
        );
    }
}
