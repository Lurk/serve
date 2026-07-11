//! Request-bucket aggregates: minute upserts, rollups, pruning, and the
//! read queries that back the dashboard (top assets, timeseries, summaries).

use super::{AssetRow, BucketTable, CountryClassRow, Dimension, Store, TimeseriesPoint, TopMetric};
use crate::errors::ServeError;
use rusqlite::OptionalExtension;

impl Store {
    /// Aggregate `src`-granularity rows in `[start, end)` into the `dst`-granularity
    /// table for dimension `dim`. Idempotent: re-running overwrites the destination
    /// bucket with a fresh SUM.
    ///
    /// # Errors
    /// Returns error if the `SQLite` statement fails.
    ///
    /// # Panics
    /// Panics if the pool cannot hand out a connection.
    pub fn rollup(
        &self,
        dim: Dimension,
        src: BucketTable,
        dst: BucketTable,
        start: i64,
        end: i64,
    ) -> Result<(), ServeError> {
        let key = dim.key_column();
        let sql = format!(
            "INSERT INTO {dst_tbl} (ts, {key}, status_class, requests, bytes)
             SELECT ?1, {key}, status_class, SUM(requests), SUM(bytes)
             FROM {src_tbl}
             WHERE ts >= ?1 AND ts < ?2
             GROUP BY {key}, status_class
             ON CONFLICT(ts, {key}, status_class) DO UPDATE SET
                 requests = excluded.requests,
                 bytes    = excluded.bytes",
            dst_tbl = dim.table(dst),
            src_tbl = dim.table(src),
        );
        let conn = self.pool.get().expect("get db connection from pool");
        conn.execute(&sql, (start, end))?;
        drop(conn);
        Ok(())
    }

    /// Delete rows older than `ts` from the `(dim, granularity)` table. Returns the
    /// number of rows deleted.
    ///
    /// # Errors
    /// Returns error if the `SQLite` delete fails.
    ///
    /// # Panics
    /// Panics if the pool cannot hand out a connection.
    pub fn prune_before(
        &self,
        dim: Dimension,
        granularity: BucketTable,
        ts: i64,
    ) -> Result<usize, ServeError> {
        let conn = self.pool.get().expect("get db connection from pool");
        let sql = format!("DELETE FROM {} WHERE ts < ?1", dim.table(granularity));
        let n = conn.execute(&sql, [ts])?;
        drop(conn);
        Ok(n)
    }

    /// Earliest `ts` present in the `(dim, granularity)` table, or `None` if empty.
    ///
    /// # Errors
    /// Returns error if the `SQLite` query fails.
    ///
    /// # Panics
    /// Panics if the pool cannot hand out a connection.
    pub fn min_ts(
        &self,
        dim: Dimension,
        granularity: BucketTable,
    ) -> Result<Option<i64>, ServeError> {
        let conn = self.pool.get().expect("get db connection from pool");
        let sql = format!("SELECT MIN(ts) FROM {}", dim.table(granularity));
        let result: Option<i64> = conn.query_row(&sql, [], |row| row.get(0))?;
        drop(conn);
        Ok(result)
    }

    /// Retrieve a metadata value by `key`.
    ///
    /// # Errors
    /// Returns error if the `SQLite` query fails.
    ///
    /// # Panics
    /// Panics if the pool cannot hand out a connection.
    pub fn meta_get(&self, key: &str) -> Result<Option<String>, ServeError> {
        let conn = self.pool.get().expect("get db connection from pool");
        let result = conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?;
        drop(conn);
        Ok(result)
    }

    /// Upsert a metadata key/value pair.
    ///
    /// # Errors
    /// Returns error if the `SQLite` query fails.
    ///
    /// # Panics
    /// Panics if the pool cannot hand out a connection.
    pub fn meta_set(&self, key: &str, value: &str) -> Result<(), ServeError> {
        let conn = self.pool.get().expect("get db connection from pool");
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            (key, value),
        )?;
        drop(conn);
        Ok(())
    }

    /// Return the top assets by `metric` from `table` since `since_ts`.
    ///
    /// # Errors
    /// Returns error if the `SQLite` query fails.
    pub fn top_assets(
        &self,
        table: BucketTable,
        since_ts: i64,
        metric: TopMetric,
        limit: u32,
    ) -> Result<Vec<AssetRow>, ServeError> {
        let order_col = match metric {
            TopMetric::Requests => "req",
            TopMetric::Bytes => "byt",
        };
        let sql = format!(
            "SELECT path, SUM(requests) AS req, SUM(bytes) AS byt
             FROM {tbl}
             WHERE ts >= ?1 AND status_class = 2
             GROUP BY path
             ORDER BY {order_col} DESC
             LIMIT ?2",
            tbl = Dimension::Path.table(table),
        );
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map((since_ts, limit), |row| {
                    Ok(AssetRow {
                        path: row.get(0)?,
                        requests: row.get(1)?,
                        bytes: row.get(2)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Return timeseries points from `table` since `since_ts`, grouped by ts and status class.
    ///
    /// # Errors
    /// Returns error if the `SQLite` query fails or a row has a `status_class` outside `u8` range.
    pub fn timeseries(
        &self,
        table: BucketTable,
        since_ts: i64,
    ) -> Result<Vec<TimeseriesPoint>, ServeError> {
        let sql = format!(
            "SELECT ts, status_class, SUM(requests), SUM(bytes)
             FROM {tbl}
             WHERE ts >= ?1
             GROUP BY ts, status_class
             ORDER BY ts, status_class",
            tbl = Dimension::Path.table(table)
        );
        let raw: Vec<(i64, i64, i64, i64)> = self.with_conn(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map([since_ts], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(ServeError::from)?;
            Ok::<Vec<_>, ServeError>(rows)
        })?;
        raw.into_iter()
            .map(|(ts, sc, requests, bytes)| {
                let status_class = u8::try_from(sc)
                    .map_err(|_| ServeError::Stats(format!("status_class={sc} out of range")))?;
                Ok(TimeseriesPoint {
                    ts,
                    status_class,
                    requests,
                    bytes,
                })
            })
            .collect()
    }

    /// Return the total (requests, bytes) for 2xx responses from `table` since `since_ts`.
    ///
    /// # Errors
    /// Returns error if the `SQLite` query fails.
    pub fn summary_2xx(&self, table: BucketTable, since_ts: i64) -> Result<(i64, i64), ServeError> {
        let sql = format!(
            "SELECT COALESCE(SUM(requests), 0), COALESCE(SUM(bytes), 0)
             FROM {tbl}
             WHERE ts >= ?1 AND status_class = 2",
            tbl = Dimension::Path.table(table)
        );
        self.with_conn(|conn| {
            let (r, b): (i64, i64) =
                conn.query_row(&sql, [since_ts], |row| Ok((row.get(0)?, row.get(1)?)))?;
            Ok((r, b))
        })
    }

    /// Return per-status-class (class, requests, bytes) from `table` since `since_ts`.
    ///
    /// # Errors
    /// Returns error if the `SQLite` query fails or a row has a `status_class` outside `u8` range.
    pub fn status_class_summary(
        &self,
        table: BucketTable,
        since_ts: i64,
    ) -> Result<Vec<(u8, i64, i64)>, ServeError> {
        let sql = format!(
            "SELECT status_class, SUM(requests), SUM(bytes)
             FROM {tbl}
             WHERE ts >= ?1
             GROUP BY status_class",
            tbl = Dimension::Path.table(table)
        );
        let raw: Vec<(i64, i64, i64)> = self.with_conn(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map([since_ts], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(ServeError::from)?;
            Ok::<Vec<_>, ServeError>(rows)
        })?;
        raw.into_iter()
            .map(|(sc, r, b)| {
                let class = u8::try_from(sc)
                    .map_err(|_| ServeError::Stats(format!("status_class={sc} out of range")))?;
                Ok((class, r, b))
            })
            .collect()
    }

    /// Upsert a batch of `(ts, key, status_class, requests, bytes)` rows into the
    /// `dim` minute table, accumulating requests and bytes on conflict.
    ///
    /// # Errors
    /// Returns error if any counter value exceeds `i64::MAX`, or if the
    /// `SQLite` transaction fails.
    pub fn upsert_minute(
        &self,
        dim: Dimension,
        rows: &[(i64, smol_str::SmolStr, u8, u64, u64)],
    ) -> Result<(), ServeError> {
        let key = dim.key_column();
        let sql = format!(
            "INSERT INTO {tbl} (ts, {key}, status_class, requests, bytes)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(ts, {key}, status_class) DO UPDATE SET
                 requests = requests + excluded.requests,
                 bytes    = bytes    + excluded.bytes",
            tbl = dim.table(BucketTable::Minute),
        );
        self.with_conn(|conn| {
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare(&sql)?;
                for (ts, key_val, status_class, requests, bytes) in rows {
                    let req_i64 = i64::try_from(*requests).map_err(|_| {
                        ServeError::Stats(format!("requests={requests} exceeds i64::MAX"))
                    })?;
                    let bytes_i64 = i64::try_from(*bytes).map_err(|_| {
                        ServeError::Stats(format!("bytes={bytes} exceeds i64::MAX"))
                    })?;
                    stmt.execute((
                        *ts,
                        key_val.as_str(),
                        i64::from(*status_class),
                        req_i64,
                        bytes_i64,
                    ))?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Return per-`(country, status_class)` totals from `table` since `since_ts`.
    /// The caller ranks and truncates to a top-N.
    ///
    /// # Errors
    /// Returns error if the `SQLite` query fails or a `status_class` is out of
    /// `u8` range.
    pub fn country_breakdown(
        &self,
        table: BucketTable,
        since_ts: i64,
    ) -> Result<Vec<CountryClassRow>, ServeError> {
        let sql = format!(
            "SELECT country, status_class, SUM(requests), SUM(bytes)
             FROM {tbl}
             WHERE ts >= ?1
             GROUP BY country, status_class",
            tbl = Dimension::Country.table(table)
        );
        let raw: Vec<(String, i64, i64, i64)> = self.with_conn(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map([since_ts], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(ServeError::from)?;
            Ok::<Vec<_>, ServeError>(rows)
        })?;
        raw.into_iter()
            .map(|(country, sc, requests, bytes)| {
                let status_class = u8::try_from(sc)
                    .map_err(|_| ServeError::Stats(format!("status_class={sc} out of range")))?;
                Ok(CountryClassRow {
                    country,
                    status_class,
                    requests,
                    bytes,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::stats::store::{BucketTable, Dimension, Store, TopMetric};
    use tempfile::tempdir;

    fn seed_for_dashboard(store: &Store) {
        store
            .upsert_minute(
                Dimension::Path,
                &[
                    (100, "/a.js".into(), 2, 10, 1000),
                    (100, "/b.js".into(), 2, 3, 300),
                    (100, "/wp-admin".into(), 4, 50, 0),
                    (160, "/a.js".into(), 2, 5, 500),
                    (160, "/b.js".into(), 2, 7, 700),
                ],
            )
            .unwrap();
    }

    #[test]
    fn top_assets_by_bytes_excludes_non_2xx() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("stats.db")).unwrap();
        seed_for_dashboard(&store);
        let rows = store
            .top_assets(BucketTable::Minute, 0, TopMetric::Bytes, 30)
            .unwrap();
        assert!(rows.iter().all(|r| r.path != "/wp-admin"));
        assert_eq!(rows[0].path, "/a.js");
        assert_eq!(rows[0].bytes, 1500);
        assert_eq!(rows[1].path, "/b.js");
    }

    #[test]
    fn top_assets_by_requests_sort_differs() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("stats.db")).unwrap();
        store
            .upsert_minute(
                Dimension::Path,
                &[
                    (100, "/a".into(), 2, 1, 10_000),
                    (100, "/b".into(), 2, 100, 100),
                ],
            )
            .unwrap();
        let by_bytes = store
            .top_assets(BucketTable::Minute, 0, TopMetric::Bytes, 30)
            .unwrap();
        let by_req = store
            .top_assets(BucketTable::Minute, 0, TopMetric::Requests, 30)
            .unwrap();
        assert_eq!(by_bytes[0].path, "/a");
        assert_eq!(by_req[0].path, "/b");
    }

    #[test]
    fn timeseries_groups_by_ts_and_class() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("stats.db")).unwrap();
        seed_for_dashboard(&store);
        let points = store.timeseries(BucketTable::Minute, 0).unwrap();
        let p = points
            .iter()
            .find(|p| p.ts == 100 && p.status_class == 2)
            .unwrap();
        assert_eq!(p.requests, 13);
        assert_eq!(p.bytes, 1300);
        let p4 = points
            .iter()
            .find(|p| p.ts == 100 && p.status_class == 4)
            .unwrap();
        assert_eq!(p4.requests, 50);
    }

    #[test]
    fn summary_2xx_totals() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("stats.db")).unwrap();
        seed_for_dashboard(&store);
        let (req, bytes) = store.summary_2xx(BucketTable::Minute, 0).unwrap();
        assert_eq!(req, 25); // 10+3+5+7
        assert_eq!(bytes, 2500);
    }

    #[test]
    fn status_class_summary() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("stats.db")).unwrap();
        seed_for_dashboard(&store);
        let per_class = store.status_class_summary(BucketTable::Minute, 0).unwrap();
        let (_, req2, bytes2) = *per_class.iter().find(|(c, _, _)| *c == 2).unwrap();
        let (_, req4, bytes4) = *per_class.iter().find(|(c, _, _)| *c == 4).unwrap();
        assert_eq!(req2, 25);
        assert_eq!(bytes2, 2500);
        assert_eq!(req4, 50);
        assert_eq!(bytes4, 0);
    }

    #[test]
    fn upsert_minute_inserts_new_row() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("stats.db")).unwrap();
        store
            .upsert_minute(
                Dimension::Path,
                &[(1_700_000_000, "/a.js".into(), 2, 5, 1024)],
            )
            .unwrap();
        let (r, b): (i64, i64) = {
            let conn = store.pool.get().unwrap();
            conn.query_row(
                "SELECT requests, bytes FROM bucket_minute WHERE ts=?1 AND path=?2 AND status_class=?3",
                (1_700_000_000_i64, "/a.js", 2_i64),
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(r, 5);
        assert_eq!(b, 1024);
    }

    #[test]
    fn upsert_minute_accumulates_on_conflict() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("stats.db")).unwrap();
        store
            .upsert_minute(Dimension::Path, &[(1000, "/a".into(), 2, 3, 100)])
            .unwrap();
        store
            .upsert_minute(Dimension::Path, &[(1000, "/a".into(), 2, 4, 200)])
            .unwrap();
        let (r, b): (i64, i64) = {
            let conn = store.pool.get().unwrap();
            conn.query_row(
                "SELECT requests, bytes FROM bucket_minute WHERE ts=1000 AND path='/a' AND status_class=2",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(r, 7);
        assert_eq!(b, 300);
    }

    #[test]
    fn rollup_hour_aggregates_minute_to_hour_idempotent() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("stats.db")).unwrap();
        store
            .upsert_minute(
                Dimension::Path,
                &[
                    (3600, "/a".into(), 2, 1, 10),
                    (3660, "/a".into(), 2, 2, 20),
                    (5400, "/b".into(), 4, 7, 0),
                ],
            )
            .unwrap();
        store
            .rollup(
                Dimension::Path,
                BucketTable::Minute,
                BucketTable::Hour,
                3600,
                7200,
            )
            .unwrap();
        let ((r, b), (r2,)): ((i64, i64), (i64,)) = {
            let conn = store.pool.get().unwrap();
            let rb = conn
                .query_row(
                    "SELECT requests, bytes FROM bucket_hour WHERE ts=3600 AND path='/a' AND status_class=2",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            let r2 = conn
                .query_row(
                    "SELECT requests FROM bucket_hour WHERE ts=3600 AND path='/b' AND status_class=4",
                    [],
                    |row| Ok((row.get(0)?,)),
                )
                .unwrap();
            drop(conn);
            (rb, r2)
        };
        assert_eq!(r, 3);
        assert_eq!(b, 30);
        assert_eq!(r2, 7);

        // Running again must be idempotent.
        store
            .rollup(
                Dimension::Path,
                BucketTable::Minute,
                BucketTable::Hour,
                3600,
                7200,
            )
            .unwrap();
        let (r, b): (i64, i64) = {
            let conn = store.pool.get().unwrap();
            conn.query_row(
                "SELECT requests, bytes FROM bucket_hour WHERE ts=3600 AND path='/a' AND status_class=2",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(r, 3, "second rollup must overwrite, not add");
        assert_eq!(b, 30);
    }

    #[test]
    fn rollup_day_aggregates_hour_to_day_idempotent() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("stats.db")).unwrap();
        {
            let conn = store.pool.get().unwrap();
            conn.execute_batch(
                "INSERT INTO bucket_hour (ts, path, status_class, requests, bytes) VALUES
             (86400, '/x', 2, 5, 50),
             (90000, '/x', 2, 3, 30),
             (90000, '/x', 5, 1,  0);",
            )
            .unwrap();
        }

        store
            .rollup(
                Dimension::Path,
                BucketTable::Hour,
                BucketTable::Day,
                86_400,
                172_800,
            )
            .unwrap();
        store
            .rollup(
                Dimension::Path,
                BucketTable::Hour,
                BucketTable::Day,
                86_400,
                172_800,
            )
            .unwrap();

        let ((r, b), (r2,)): ((i64, i64), (i64,)) = {
            let conn = store.pool.get().unwrap();
            let rb = conn
                .query_row(
                    "SELECT requests, bytes FROM bucket_day WHERE ts=86400 AND path='/x' AND status_class=2",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            let r2 = conn
                .query_row(
                    "SELECT requests FROM bucket_day WHERE ts=86400 AND path='/x' AND status_class=5",
                    [],
                    |row| Ok((row.get(0)?,)),
                )
                .unwrap();
            drop(conn);
            (rb, r2)
        };
        assert_eq!(r, 8);
        assert_eq!(b, 80);
        assert_eq!(r2, 1);
    }

    #[test]
    fn prune_buckets_older_than() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("stats.db")).unwrap();
        store
            .upsert_minute(
                Dimension::Path,
                &[
                    (100, "/a".into(), 2, 1, 1),
                    (200, "/a".into(), 2, 1, 1),
                    (300, "/a".into(), 2, 1, 1),
                ],
            )
            .unwrap();
        let removed = store
            .prune_before(Dimension::Path, BucketTable::Minute, 200)
            .unwrap();
        assert_eq!(removed, 1);
        let count: i64 = {
            let conn = store.pool.get().unwrap();
            conn.query_row("SELECT COUNT(*) FROM bucket_minute", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(count, 2);
    }

    #[test]
    fn meta_get_set() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("stats.db")).unwrap();
        assert!(store.meta_get("last_hour_rollup_ts").unwrap().is_none());
        store.meta_set("last_hour_rollup_ts", "3600").unwrap();
        assert_eq!(
            store.meta_get("last_hour_rollup_ts").unwrap().as_deref(),
            Some("3600")
        );
        store.meta_set("last_hour_rollup_ts", "7200").unwrap();
        assert_eq!(
            store.meta_get("last_hour_rollup_ts").unwrap().as_deref(),
            Some("7200")
        );
    }

    #[test]
    fn country_minute_upsert_and_breakdown() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.db")).unwrap();
        store
            .upsert_minute(
                Dimension::Country,
                &[
                    (60, "US".into(), 2, 3, 300),
                    (60, "US".into(), 4, 1, 40),
                    (60, "DE".into(), 2, 5, 500),
                ],
            )
            .unwrap();
        // Upsert again to confirm additive accumulation on conflict.
        store
            .upsert_minute(Dimension::Country, &[(60, "US".into(), 2, 2, 100)])
            .unwrap();

        let rows = store.country_breakdown(BucketTable::Minute, 0).unwrap();
        let us2 = rows
            .iter()
            .find(|r| r.country == "US" && r.status_class == 2)
            .unwrap();
        assert_eq!(us2.requests, 5);
        assert_eq!(us2.bytes, 400);
        let de2 = rows
            .iter()
            .find(|r| r.country == "DE" && r.status_class == 2)
            .unwrap();
        assert_eq!(de2.requests, 5);
        assert_eq!(de2.bytes, 500);
    }

    #[test]
    fn country_rollup_hour_and_day_aggregate() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.db")).unwrap();
        store
            .upsert_minute(
                Dimension::Country,
                &[(3600, "US".into(), 2, 5, 50), (4200, "US".into(), 2, 2, 20)],
            )
            .unwrap();
        store
            .rollup(
                Dimension::Country,
                BucketTable::Minute,
                BucketTable::Hour,
                3600,
                7200,
            )
            .unwrap();
        // Idempotent re-run.
        store
            .rollup(
                Dimension::Country,
                BucketTable::Minute,
                BucketTable::Hour,
                3600,
                7200,
            )
            .unwrap();
        let hour = store.country_breakdown(BucketTable::Hour, 0).unwrap();
        let us = hour.iter().find(|r| r.country == "US").unwrap();
        assert_eq!(us.requests, 7);
        assert_eq!(us.bytes, 70);

        store
            .rollup(
                Dimension::Country,
                BucketTable::Hour,
                BucketTable::Day,
                0,
                86_400,
            )
            .unwrap();
        let day = store.country_breakdown(BucketTable::Day, 0).unwrap();
        let us = day.iter().find(|r| r.country == "US").unwrap();
        assert_eq!(us.requests, 7);
        assert_eq!(us.bytes, 70);
    }

    #[test]
    fn country_prune_removes_old_rows() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.db")).unwrap();
        store
            .upsert_minute(
                Dimension::Country,
                &[
                    (60, "US".into(), 2, 1, 1),
                    (1_000_000, "US".into(), 2, 1, 1),
                ],
            )
            .unwrap();
        let removed = store
            .prune_before(Dimension::Country, BucketTable::Minute, 1000)
            .unwrap();
        assert_eq!(removed, 1);
        let rows = store.country_breakdown(BucketTable::Minute, 0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].requests, 1);
    }
}
