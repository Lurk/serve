//! Password hash and session-token storage.

use super::Store;
use crate::errors::ServeError;
use rusqlite::OptionalExtension;

impl Store {
    /// Retrieve the stored password hash, if any.
    ///
    /// # Errors
    /// Returns error if the `SQLite` query fails.
    ///
    /// # Panics
    /// Panics if the pool cannot hand out a connection.
    pub fn password_hash(&self) -> Result<Option<String>, ServeError> {
        let conn = self.pool.get().expect("get db connection from pool");
        let result = conn
            .query_row(
                "SELECT password_hash FROM stats_auth WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        drop(conn);
        Ok(result)
    }

    /// Persist `hash` as the stats password (upsert on id=1).
    ///
    /// # Errors
    /// Returns error if the `SQLite` insert fails.
    ///
    /// # Panics
    /// Panics if the pool cannot hand out a connection.
    pub fn set_password_hash(&self, hash: &str, created_at: i64) -> Result<(), ServeError> {
        let conn = self.pool.get().expect("get db connection from pool");
        conn.execute(
            "INSERT INTO stats_auth (id, password_hash, created_at) VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET password_hash = excluded.password_hash,
                                           created_at    = excluded.created_at",
            (hash, created_at),
        )?;
        drop(conn);
        Ok(())
    }

    /// Insert a new session row.
    ///
    /// # Errors
    /// Returns error if inserting the session row fails.
    ///
    /// # Panics
    /// Panics if the pool cannot hand out a connection.
    pub fn create_session(
        &self,
        token: &str,
        created_at: i64,
        expires_at: i64,
    ) -> Result<(), ServeError> {
        let conn = self.pool.get().expect("get db connection from pool");
        conn.execute(
            "INSERT INTO sessions (token, created_at, expires_at) VALUES (?1, ?2, ?3)",
            (token, created_at, expires_at),
        )?;
        drop(conn);
        Ok(())
    }

    /// Check whether `token` exists and has not expired at `now`.
    ///
    /// # Errors
    /// Returns error if the `SQLite` query fails.
    ///
    /// # Panics
    /// Panics if the pool cannot hand out a connection.
    pub fn session_valid(&self, token: &str, now: i64) -> Result<bool, ServeError> {
        let conn = self.pool.get().expect("get db connection from pool");
        let result = conn
            .query_row(
                "SELECT expires_at FROM sessions WHERE token = ?1",
                [token],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        drop(conn);
        Ok(result.is_some_and(|exp| exp >= now))
    }

    /// Delete the session row for `token`.
    ///
    /// # Errors
    /// Returns error if the `SQLite` delete fails.
    ///
    /// # Panics
    /// Panics if the pool cannot hand out a connection.
    pub fn delete_session(&self, token: &str) -> Result<(), ServeError> {
        let conn = self.pool.get().expect("get db connection from pool");
        conn.execute("DELETE FROM sessions WHERE token = ?1", [token])?;
        drop(conn);
        Ok(())
    }

    /// Delete all sessions that have expired before `now`. Returns the number deleted.
    ///
    /// # Errors
    /// Returns error if the `SQLite` delete fails.
    ///
    /// # Panics
    /// Panics if the pool cannot hand out a connection.
    pub fn prune_expired_sessions(&self, now: i64) -> Result<usize, ServeError> {
        let conn = self.pool.get().expect("get db connection from pool");
        let n = conn.execute("DELETE FROM sessions WHERE expires_at < ?1", [now])?;
        drop(conn);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use crate::stats::store::Store;
    use tempfile::tempdir;

    #[test]
    fn auth_set_then_get_password_hash() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("stats.db")).unwrap();
        assert!(store.password_hash().unwrap().is_none());

        store
            .set_password_hash("phc_string_here", 1_700_000_000)
            .unwrap();
        let got = store.password_hash().unwrap();
        assert_eq!(got.as_deref(), Some("phc_string_here"));
    }

    #[test]
    fn auth_set_password_hash_twice_replaces() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("stats.db")).unwrap();
        store.set_password_hash("first", 1).unwrap();
        store.set_password_hash("second", 2).unwrap();
        assert_eq!(store.password_hash().unwrap().as_deref(), Some("second"));
    }

    #[test]
    fn session_create_and_validate() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("stats.db")).unwrap();
        store.create_session("tok-abc", 1000, 1000 + 60).unwrap();
        assert!(store.session_valid("tok-abc", 1030).unwrap());
        assert!(!store.session_valid("tok-abc", 1061).unwrap());
        assert!(!store.session_valid("nope", 1000).unwrap());
    }

    #[test]
    fn session_delete() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("stats.db")).unwrap();
        store.create_session("tok", 1000, 9999).unwrap();
        assert!(store.session_valid("tok", 1500).unwrap());
        store.delete_session("tok").unwrap();
        assert!(!store.session_valid("tok", 1500).unwrap());
    }

    #[test]
    fn session_prune_expired() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("stats.db")).unwrap();
        store.create_session("expired", 100, 200).unwrap();
        store.create_session("alive", 100, 9999).unwrap();
        let removed = store.prune_expired_sessions(500).unwrap();
        assert_eq!(removed, 1);
        assert!(!store.session_valid("expired", 500).unwrap());
        assert!(store.session_valid("alive", 500).unwrap());
    }
}
