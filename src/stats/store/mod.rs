//! `SQLite`-backed stats store. The `Store` type and lifecycle live here;
//! query methods are split across sibling modules by concern:
//! - [`auth`] — password + session rows
//! - [`buckets`] — minute/hour/day request aggregates and rollups
//! - [`migrations`] — schema migration list
//! - [`types`] — row structs and dimension/granularity enums

use crate::errors::ServeError;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;
use std::path::Path;

mod auth;
mod buckets;
mod migrations;
mod source;
mod types;

pub use types::{AssetRow, BucketTable, CountryClassRow, Dimension, TimeseriesPoint, TopMetric};

use migrations::MIGRATIONS;

pub struct Store {
    pool: Pool<SqliteConnectionManager>,
}

/// Max concurrent connections to the `SQLite` file. Writes serialize on a single
/// `SQLite` writer regardless; this only opens parallelism for readers.
const POOL_MAX_SIZE: u32 = 8;

impl Store {
    /// Open the store at `db_path`, creating parent dirs and running migrations.
    ///
    /// # Errors
    /// Returns error if creating parent dir, opening the database, or running migrations fails.
    pub fn open(db_path: &Path) -> Result<Self, ServeError> {
        if let Some(parent) = db_path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)?;
        }
        let manager = SqliteConnectionManager::file(db_path).with_init(|c| {
            // WAL is sticky on the file once set, but re-issuing on each new
            // connection is harmless and ensures synchronous=NORMAL is applied
            // per-connection (synchronous is a connection-level pragma).
            c.pragma_update(None, "journal_mode", "WAL")?;
            c.pragma_update(None, "synchronous", "NORMAL")
        });
        let pool = Pool::builder().max_size(POOL_MAX_SIZE).build(manager)?;
        let store = Self { pool };
        store.run_migrations()?;
        Ok(store)
    }

    /// Run `f` with a pooled `SQLite` connection. The connection is returned to
    /// the pool when `f` returns.
    ///
    /// # Panics
    /// Panics if the pool cannot hand out a connection (timeout or pool-init
    /// failure on the underlying `SQLite` manager).
    pub fn with_conn<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Connection) -> R,
    {
        let mut conn = self.pool.get().expect("get db connection from pool");
        f(&mut conn)
    }

    fn run_migrations(&self) -> Result<(), ServeError> {
        let mut conn = self.pool.get()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY);",
        )?;
        let current: i64 = conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )?;
        for (version, sql) in MIGRATIONS {
            if *version > current {
                // Each migration's DDL batch and its version row commit together,
                // so a mid-migration failure rolls back cleanly and the retry
                // re-runs the whole migration instead of hitting "already exists".
                let tx = conn.transaction()?;
                tx.execute_batch(sql)?;
                tx.execute(
                    "INSERT INTO schema_migrations(version) VALUES (?1)",
                    [version],
                )?;
                tx.commit()?;
            }
        }
        drop(conn);
        Ok(())
    }

    /// # Panics
    /// Panics if the pool cannot hand out a connection or the DROP TABLE fails.
    #[cfg(test)]
    pub fn drop_bucket_minute_for_test(&self) {
        let conn = self.pool.get().expect("get db connection from pool");
        conn.execute_batch("DROP TABLE bucket_minute")
            .expect("drop bucket_minute");
    }

    /// Returns the current migration schema version.
    ///
    /// # Errors
    /// Returns error if the `SQLite` query fails.
    ///
    /// # Panics
    /// Panics if the pool cannot hand out a connection.
    #[cfg(test)]
    pub fn schema_version(&self) -> Result<i64, ServeError> {
        let conn = self.pool.get().expect("get db connection from pool");
        let v: i64 = conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )?;
        drop(conn);
        Ok(v)
    }

    /// # Panics
    /// Panics if the pool cannot hand out a connection.
    #[cfg(test)]
    #[must_use]
    pub fn conn_for_test(&self) -> r2d2::PooledConnection<SqliteConnectionManager> {
        self.pool.get().expect("get db connection from pool")
    }
}

#[cfg(test)]
mod tests {
    use crate::stats::store::Store;
    use tempfile::tempdir;

    #[test]
    fn opens_fresh_db() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("stats.db");
        let store = Store::open(&db).unwrap();
        assert!(store.schema_version().unwrap() >= 1);
    }

    #[test]
    fn open_creates_parent_directory() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("nested/sub/stats.db");
        let store = Store::open(&db).unwrap();
        assert!(store.schema_version().unwrap() >= 1);
        assert!(db.exists());
    }
}
