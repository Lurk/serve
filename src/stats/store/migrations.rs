//! Ordered schema migration list, applied by `Store::run_migrations`.

pub(super) const MIGRATIONS: &[(i64, &str)] = &[
    (
        1,
        r"
    CREATE TABLE stats_auth (
        id            INTEGER PRIMARY KEY CHECK (id = 1),
        password_hash TEXT    NOT NULL,
        created_at    INTEGER NOT NULL
    );
    CREATE TABLE sessions (
        token       TEXT    PRIMARY KEY,
        created_at  INTEGER NOT NULL,
        expires_at  INTEGER NOT NULL
    );
    CREATE INDEX idx_sessions_expires ON sessions(expires_at);
    ",
    ),
    (
        2,
        r"
        CREATE TABLE bucket_minute (
            ts            INTEGER NOT NULL,
            path          TEXT    NOT NULL,
            status_class  INTEGER NOT NULL,
            requests      INTEGER NOT NULL,
            bytes         INTEGER NOT NULL,
            PRIMARY KEY (ts, path, status_class)
        ) WITHOUT ROWID;
        CREATE INDEX idx_bm_ts ON bucket_minute(ts);

        CREATE TABLE bucket_hour (
            ts            INTEGER NOT NULL,
            path          TEXT    NOT NULL,
            status_class  INTEGER NOT NULL,
            requests      INTEGER NOT NULL,
            bytes         INTEGER NOT NULL,
            PRIMARY KEY (ts, path, status_class)
        ) WITHOUT ROWID;
        CREATE INDEX idx_bh_ts ON bucket_hour(ts);

        CREATE TABLE bucket_day (
            ts            INTEGER NOT NULL,
            path          TEXT    NOT NULL,
            status_class  INTEGER NOT NULL,
            requests      INTEGER NOT NULL,
            bytes         INTEGER NOT NULL,
            PRIMARY KEY (ts, path, status_class)
        ) WITHOUT ROWID;
        CREATE INDEX idx_bd_ts ON bucket_day(ts);
        ",
    ),
    (
        3,
        r"
        CREATE TABLE meta (
            key    TEXT NOT NULL PRIMARY KEY,
            value  TEXT NOT NULL
        );
        ",
    ),
    (
        4,
        r"
        -- top_assets filters by (status_class, ts >= ?) and GROUPs BY path. The
        -- existing PK (ts, path, status_class) forces a full range scan; this
        -- composite index lets SQLite seek directly to the matching slice.
        CREATE INDEX idx_bm_class_ts_path ON bucket_minute(status_class, ts, path);
        CREATE INDEX idx_bh_class_ts_path ON bucket_hour(status_class, ts, path);
        CREATE INDEX idx_bd_class_ts_path ON bucket_day(status_class, ts, path);
        ",
    ),
    (
        5,
        r"
        CREATE TABLE country_minute (
            ts            INTEGER NOT NULL,
            country       TEXT    NOT NULL,
            status_class  INTEGER NOT NULL,
            requests      INTEGER NOT NULL,
            bytes         INTEGER NOT NULL,
            PRIMARY KEY (ts, country, status_class)
        ) WITHOUT ROWID;
        CREATE INDEX idx_cm_ts ON country_minute(ts);

        CREATE TABLE country_hour (
            ts            INTEGER NOT NULL,
            country       TEXT    NOT NULL,
            status_class  INTEGER NOT NULL,
            requests      INTEGER NOT NULL,
            bytes         INTEGER NOT NULL,
            PRIMARY KEY (ts, country, status_class)
        ) WITHOUT ROWID;
        CREATE INDEX idx_ch_ts ON country_hour(ts);

        CREATE TABLE country_day (
            ts            INTEGER NOT NULL,
            country       TEXT    NOT NULL,
            status_class  INTEGER NOT NULL,
            requests      INTEGER NOT NULL,
            bytes         INTEGER NOT NULL,
            PRIMARY KEY (ts, country, status_class)
        ) WITHOUT ROWID;
        CREATE INDEX idx_cd_ts ON country_day(ts);
        ",
    ),
];

#[cfg(test)]
mod tests {
    use crate::stats::store::Store;
    use tempfile::tempdir;

    #[test]
    fn migration_1_creates_auth_tables() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("stats.db");
        let store = Store::open(&db).unwrap();
        assert!(store.schema_version().unwrap() >= 1);
        for table in ["stats_auth", "sessions"] {
            let count: i64 = store
                .conn_for_test()
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "missing table {table}");
        }
    }

    #[test]
    fn migration_2_creates_bucket_tables() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("stats.db");
        let store = Store::open(&db).unwrap();
        assert!(store.schema_version().unwrap() >= 2);
        for table in ["bucket_minute", "bucket_hour", "bucket_day"] {
            let count: i64 = store
                .conn_for_test()
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "missing table {table}");
        }
    }

    #[test]
    fn migration_3_creates_meta_table() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("stats.db");
        let store = Store::open(&db).unwrap();
        assert!(store.schema_version().unwrap() >= 3);
        let count: i64 = store
            .conn_for_test()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='meta'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn migration_4_creates_top_assets_indexes() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("stats.db");
        let store = Store::open(&db).unwrap();
        assert!(store.schema_version().unwrap() >= 4);
        for name in [
            "idx_bm_class_ts_path",
            "idx_bh_class_ts_path",
            "idx_bd_class_ts_path",
        ] {
            let count: i64 = store
                .conn_for_test()
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
                    [name],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "missing index {name}");
        }
    }

    #[test]
    fn migration_v5_creates_country_tables() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.db")).unwrap();
        assert_eq!(store.schema_version().unwrap(), 5);
        let conn = store.conn_for_test();
        for tbl in ["country_minute", "country_hour", "country_day"] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [tbl],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "missing table {tbl}");
        }
        drop(conn);
    }

    #[test]
    fn migrations_are_idempotent() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("stats.db");
        let v1 = Store::open(&db).unwrap().schema_version().unwrap();
        let v2 = Store::open(&db).unwrap().schema_version().unwrap();
        assert_eq!(v1, v2);
    }
}
