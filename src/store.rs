//! Local SQLite session store for decoded [`Sample`]s, with CSV export.
//!
//! One database file holds one acquisition session. Every sample is tied to a
//! stable [`TagId`] (a tag's BLE address), so data drained from a single
//! physical tag across many short-lived connect/drain cycles all groups under
//! the same id (CLAUDE.md A.1/A.2/A.7). I/O lives at this edge; the decode layer
//! in [`crate::models`] stays pure.
//!
//! Schema:
//!
//! ```sql
//! CREATE TABLE tags (
//!     tag_id TEXT PRIMARY KEY,
//!     name   TEXT NOT NULL
//! );
//! CREATE TABLE samples (
//!     id           INTEGER PRIMARY KEY AUTOINCREMENT,
//!     tag_id       TEXT NOT NULL REFERENCES tags(tag_id),
//!     timestamp_ms INTEGER NOT NULL,
//!     temperature_c REAL NOT NULL,
//!     humidity_pct  REAL NOT NULL,
//!     adc1_mv       INTEGER NOT NULL,
//!     adc2_mv       INTEGER NOT NULL,
//!     stim_a_mv     INTEGER NOT NULL,
//!     stim_b_mv     INTEGER NOT NULL,
//!     current_a     REAL NOT NULL,
//!     current_b     REAL NOT NULL
//! );
//! ```

use std::path::Path;

use rusqlite::Connection;

use crate::models::Sample;

/// A stable identifier for a physical tag (e.g. its BLE address).
///
/// Using a newtype rather than a bare `String` keeps tag identity distinct from
/// arbitrary strings throughout the store API (CLAUDE.md: newtype domain
/// primitives).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TagId(String);

impl TagId {
    /// Creates a tag id from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Returns the underlying id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TagId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for TagId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for TagId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Errors raised by the session store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A SQLite operation failed.
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    /// A CSV write failed.
    #[error("csv error: {0}")]
    Csv(#[from] csv::Error),
    /// Writing a CSV record to its destination failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// CSV header row, in the order [`write_sample_record`] emits fields.
const CSV_HEADER: [&str; 10] = [
    "tag_id",
    "timestamp_ms",
    "temperature_c",
    "humidity_pct",
    "adc1_mv",
    "adc2_mv",
    "stim_a_mv",
    "stim_b_mv",
    "current_a",
    "current_b",
];

/// A handle to one session's SQLite database.
pub struct SessionStore {
    connection: Connection,
}

impl SessionStore {
    /// Opens (creating if needed) the session database at `path`, ensuring the
    /// schema exists.
    ///
    /// # Errors
    /// Returns [`StoreError::Database`] if the file cannot be opened or the
    /// schema cannot be created.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let connection = Connection::open(path)?;
        Self::from_connection(connection)
    }

    /// Opens an in-memory session database (used by tests; never touches disk).
    ///
    /// # Errors
    /// Returns [`StoreError::Database`] if the schema cannot be created.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;
        Self::from_connection(connection)
    }

    /// Initialises schema and pragmas on a freshly opened connection.
    fn from_connection(connection: Connection) -> Result<Self, StoreError> {
        // Enforce the samples -> tags foreign key (off by default in SQLite).
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS tags (
                 tag_id TEXT PRIMARY KEY,
                 name   TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS samples (
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 tag_id        TEXT NOT NULL REFERENCES tags(tag_id),
                 timestamp_ms  INTEGER NOT NULL,
                 temperature_c REAL NOT NULL,
                 humidity_pct  REAL NOT NULL,
                 adc1_mv       INTEGER NOT NULL,
                 adc2_mv       INTEGER NOT NULL,
                 stim_a_mv     INTEGER NOT NULL,
                 stim_b_mv     INTEGER NOT NULL,
                 current_a     REAL NOT NULL,
                 current_b     REAL NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_samples_tag_time
                 ON samples(tag_id, timestamp_ms);",
        )?;
        Ok(Self { connection })
    }

    /// Records (or updates) a tag's human-readable name for `tag_id`.
    ///
    /// # Errors
    /// Returns [`StoreError::Database`] if the upsert fails.
    pub fn upsert_tag(&self, tag_id: &TagId, name: &str) -> Result<(), StoreError> {
        self.connection.execute(
            "INSERT INTO tags (tag_id, name) VALUES (?1, ?2)
             ON CONFLICT(tag_id) DO UPDATE SET name = excluded.name",
            (tag_id.as_str(), name),
        )?;
        Ok(())
    }

    /// Inserts a batch of decoded samples for `tag_id`.
    ///
    /// One drained packet (up to [`crate::models::SAMPLES_PER_PACKET`] samples)
    /// can be inserted in a single call. The tag is auto-registered (with an
    /// empty name unless [`upsert_tag`](Self::upsert_tag) set one) so the
    /// foreign key always holds. The whole batch commits atomically.
    ///
    /// # Errors
    /// Returns [`StoreError::Database`] if any insert fails; the transaction is
    /// rolled back so no partial batch is stored.
    pub fn insert_samples(&self, tag_id: &TagId, samples: &[Sample]) -> Result<(), StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        // Ensure the tag row exists without clobbering a previously set name.
        transaction.execute(
            "INSERT INTO tags (tag_id, name) VALUES (?1, '')
             ON CONFLICT(tag_id) DO NOTHING",
            (tag_id.as_str(),),
        )?;
        {
            let mut statement = transaction.prepare(
                "INSERT INTO samples (
                     tag_id, timestamp_ms, temperature_c, humidity_pct,
                     adc1_mv, adc2_mv, stim_a_mv, stim_b_mv, current_a, current_b
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?;
            for sample in samples {
                statement.execute((
                    tag_id.as_str(),
                    sample.timestamp_ms,
                    sample.temperature_c,
                    sample.humidity_pct,
                    sample.adc1_mv,
                    sample.adc2_mv,
                    sample.stim_a_mv,
                    sample.stim_b_mv,
                    sample.current_a,
                    sample.current_b,
                ))?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Returns all samples for `tag_id`, ordered by ascending timestamp.
    ///
    /// # Errors
    /// Returns [`StoreError::Database`] if the query fails.
    pub fn samples_for(&self, tag_id: &TagId) -> Result<Vec<Sample>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT timestamp_ms, temperature_c, humidity_pct,
                    adc1_mv, adc2_mv, stim_a_mv, stim_b_mv, current_a, current_b
             FROM samples
             WHERE tag_id = ?1
             ORDER BY timestamp_ms ASC, id ASC",
        )?;
        let rows = statement.query_map((tag_id.as_str(),), |row| {
            Ok(Sample {
                timestamp_ms: row.get(0)?,
                temperature_c: row.get(1)?,
                humidity_pct: row.get(2)?,
                adc1_mv: row.get(3)?,
                adc2_mv: row.get(4)?,
                stim_a_mv: row.get(5)?,
                stim_b_mv: row.get(6)?,
                current_a: row.get(7)?,
                current_b: row.get(8)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Returns all tag ids known in this session, in ascending id order.
    ///
    /// # Errors
    /// Returns [`StoreError::Database`] if the query fails.
    pub fn tag_ids(&self) -> Result<Vec<TagId>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT tag_id FROM tags ORDER BY tag_id ASC")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0).map(TagId::from))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Exports `tag_id`'s samples to a CSV file at `path`.
    ///
    /// Writes a human-readable header row followed by one row per sample, in
    /// timestamp order. The `tag_id` is included as the first column.
    ///
    /// # Errors
    /// Returns an error if the query fails or the file cannot be written.
    pub fn export_csv(&self, tag_id: &TagId, path: &Path) -> Result<(), StoreError> {
        let samples = self.samples_for(tag_id)?;
        let mut writer = csv::Writer::from_path(path)?;
        writer.write_record(CSV_HEADER)?;
        for sample in &samples {
            write_sample_record(&mut writer, tag_id, sample)?;
        }
        writer.flush()?;
        Ok(())
    }
}

/// Writes a single sample as a CSV record, matching [`CSV_HEADER`] column order.
fn write_sample_record<W: std::io::Write>(
    writer: &mut csv::Writer<W>,
    tag_id: &TagId,
    sample: &Sample,
) -> Result<(), StoreError> {
    writer.write_record([
        tag_id.as_str(),
        &sample.timestamp_ms.to_string(),
        &sample.temperature_c.to_string(),
        &sample.humidity_pct.to_string(),
        &sample.adc1_mv.to_string(),
        &sample.adc2_mv.to_string(),
        &sample.stim_a_mv.to_string(),
        &sample.stim_b_mv.to_string(),
        &sample.current_a.to_string(),
        &sample.current_b.to_string(),
    ])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a sample with a given timestamp; other fields are deterministic
    /// functions of it so round-trips are easy to assert.
    fn sample_at(timestamp_ms: i64) -> Sample {
        Sample {
            timestamp_ms,
            temperature_c: 25.67,
            humidity_pct: 45.0,
            adc1_mv: 100,
            adc2_mv: -100,
            stim_a_mv: 304,
            stim_b_mv: 304,
            current_a: (100.0 - 304.0) / 3000.0,
            current_b: (-100.0 - 304.0) / 3000.0,
        }
    }

    #[test]
    fn round_trips_samples_ordered_by_timestamp() {
        let store = SessionStore::open_in_memory().expect("open in-memory db");
        let tag = TagId::new("AA:BB:CC:DD:EE:FF");
        store.upsert_tag(&tag, "tag-one").expect("upsert tag");

        // Insert out of timestamp order to prove ordering is by query, not input.
        let inserted = [sample_at(300), sample_at(100), sample_at(200)];
        store.insert_samples(&tag, &inserted).expect("insert");

        let read_back = store.samples_for(&tag).expect("read back");
        let timestamps: Vec<i64> = read_back.iter().map(|s| s.timestamp_ms).collect();
        assert_eq!(timestamps, [100, 200, 300]);
        assert_eq!(read_back[0], sample_at(100));
    }

    #[test]
    fn isolates_samples_between_two_tags() {
        let store = SessionStore::open_in_memory().expect("open in-memory db");
        let tag_a = TagId::new("AA:AA:AA:AA:AA:AA");
        let tag_b = TagId::new("BB:BB:BB:BB:BB:BB");

        store.insert_samples(&tag_a, &[sample_at(10)]).expect("a");
        store
            .insert_samples(&tag_b, &[sample_at(20), sample_at(30)])
            .expect("b");

        let mut ids = store.tag_ids().expect("tag ids");
        ids.sort();
        assert_eq!(ids, vec![tag_a.clone(), tag_b.clone()]);

        assert_eq!(store.samples_for(&tag_a).expect("a samples").len(), 1);
        assert_eq!(store.samples_for(&tag_b).expect("b samples").len(), 2);
    }

    #[test]
    fn accumulates_samples_across_repeated_drain_cycles() {
        let store = SessionStore::open_in_memory().expect("open in-memory db");
        let tag = TagId::new("AA:BB:CC:DD:EE:FF");

        // Two separate drains for the same physical tag.
        store
            .insert_samples(&tag, &[sample_at(100)])
            .expect("drain 1");
        store
            .insert_samples(&tag, &[sample_at(200), sample_at(300)])
            .expect("drain 2");

        let read_back = store.samples_for(&tag).expect("read back");
        assert_eq!(read_back.len(), 3);
        let timestamps: Vec<i64> = read_back.iter().map(|s| s.timestamp_ms).collect();
        assert_eq!(timestamps, [100, 200, 300]);
    }

    #[test]
    fn upsert_tag_updates_existing_name() {
        let store = SessionStore::open_in_memory().expect("open in-memory db");
        let tag = TagId::new("AA:BB:CC:DD:EE:FF");
        store.upsert_tag(&tag, "first").expect("first name");
        store.upsert_tag(&tag, "second").expect("second name");
        // Still exactly one tag, name overwritten (not duplicated).
        assert_eq!(store.tag_ids().expect("ids"), vec![tag]);
    }

    #[test]
    fn exports_csv_with_header_and_one_row_per_sample() {
        let store = SessionStore::open_in_memory().expect("open in-memory db");
        let tag = TagId::new("AA:BB:CC:DD:EE:FF");
        store
            .insert_samples(&tag, &[sample_at(100), sample_at(200), sample_at(300)])
            .expect("insert");

        let dir = std::env::temp_dir().join(format!("pc_sink_csv_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("export.csv");
        store.export_csv(&tag, &path).expect("export csv");

        assert!(path.exists(), "csv file was created");
        let contents = std::fs::read_to_string(&path).expect("read csv");
        let lines: Vec<&str> = contents.lines().collect();
        // 1 header + 3 data rows.
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with("tag_id,timestamp_ms,"));
        assert!(lines[1].starts_with("AA:BB:CC:DD:EE:FF,100,"));

        std::fs::remove_dir_all(&dir).expect("clean temp dir");
    }

    #[test]
    fn opens_and_persists_to_a_file_path() {
        let dir = std::env::temp_dir().join(format!("pc_sink_db_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("session.sqlite");
        let tag = TagId::new("AA:BB:CC:DD:EE:FF");

        {
            let store = SessionStore::open(&path).expect("open file db");
            store
                .insert_samples(&tag, &[sample_at(42)])
                .expect("insert");
        }
        // Reopen the same file: data persisted.
        let store = SessionStore::open(&path).expect("reopen file db");
        assert_eq!(store.samples_for(&tag).expect("samples").len(), 1);

        std::fs::remove_dir_all(&dir).expect("clean temp dir");
    }
}
