//! Per-source latency histograms: minute upserts, rollups, and read queries.
//! Source is "local" (static serving) or "proxy:<prefix>" per route. Columns
//! are 13-bucket TTFB + total histograms (see `crate::stats::latency`).
// Consumed by the recording pipeline in a later commit; unused on its own.
#![allow(dead_code)]

use super::{BucketTable, Store};
use crate::errors::ServeError;
use crate::stats::latency::N_BUCKETS;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRow {
    pub ts: i64,
    pub source: smol_str::SmolStr,
    pub requests: u64,
    pub not_modified: u64,
    pub ttfb: [u64; N_BUCKETS],
    pub total: [u64; N_BUCKETS],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceTotals {
    pub source: String,
    pub requests: i64,
    pub not_modified: i64,
    pub ttfb: [u64; N_BUCKETS],
    pub total: [u64; N_BUCKETS],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceTsRow {
    pub source: String,
    pub ts: i64,
    pub ttfb: [u64; N_BUCKETS],
    pub total: [u64; N_BUCKETS],
}

/// `"ttfb0, ttfb1, ..."` for the given prefix — used to build histogram SQL.
fn source_bucket_cols(prefix: &str) -> String {
    (0..N_BUCKETS)
        .map(|i| format!("{prefix}{i}"))
        .collect::<Vec<_>>()
        .join(", ")
}

const fn table_name(t: BucketTable) -> &'static str {
    match t {
        BucketTable::Minute => "source_minute",
        BucketTable::Hour => "source_hour",
        BucketTable::Day => "source_day",
    }
}

impl Store {
    /// Upsert per-source minute rows, accumulating all counters on conflict.
    ///
    /// # Errors
    /// Returns error if a counter exceeds `i64::MAX` or the `SQLite` tx fails.
    pub fn upsert_source(&self, rows: &[SourceRow]) -> Result<(), ServeError> {
        let ttfb = source_bucket_cols("ttfb");
        let total = source_bucket_cols("total");
        // 4 fixed params + 2*N histogram params.
        let n_params = 4 + 2 * N_BUCKETS;
        let placeholders = (1..=n_params)
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let updates = ["requests", "not_modified"]
            .into_iter()
            .map(String::from)
            .chain((0..N_BUCKETS).map(|i| format!("ttfb{i}")))
            .chain((0..N_BUCKETS).map(|i| format!("total{i}")))
            .map(|c| format!("{c} = {c} + excluded.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "INSERT INTO source_minute (ts, source, requests, not_modified, {ttfb}, {total})
             VALUES ({placeholders})
             ON CONFLICT(ts, source) DO UPDATE SET {updates}"
        );
        self.with_conn(|conn| {
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare(&sql)?;
                for r in rows {
                    let to_i64 = |v: u64, what: &str| {
                        i64::try_from(v)
                            .map_err(|_| ServeError::Stats(format!("{what}={v} exceeds i64::MAX")))
                    };
                    let mut vals: Vec<rusqlite::types::Value> =
                        Vec::with_capacity(4 + 2 * N_BUCKETS);
                    vals.push(r.ts.into());
                    vals.push(r.source.to_string().into());
                    vals.push(to_i64(r.requests, "requests")?.into());
                    vals.push(to_i64(r.not_modified, "not_modified")?.into());
                    for (i, &c) in r.ttfb.iter().enumerate() {
                        vals.push(to_i64(c, &format!("ttfb{i}"))?.into());
                    }
                    for (i, &c) in r.total.iter().enumerate() {
                        vals.push(to_i64(c, &format!("total{i}"))?.into());
                    }
                    stmt.execute(rusqlite::params_from_iter(vals))?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Aggregate `src` rows in `[start, end)` into the `dst` table. Idempotent:
    /// overwrites the destination bucket with a fresh SUM.
    ///
    /// # Errors
    /// Returns error if the `SQLite` statement fails.
    ///
    /// # Panics
    /// Panics if the pool cannot hand out a connection.
    pub fn rollup_source(
        &self,
        src: BucketTable,
        dst: BucketTable,
        start: i64,
        end: i64,
    ) -> Result<(), ServeError> {
        let cols = ["requests", "not_modified"]
            .into_iter()
            .map(String::from)
            .chain((0..N_BUCKETS).map(|i| format!("ttfb{i}")))
            .chain((0..N_BUCKETS).map(|i| format!("total{i}")))
            .collect::<Vec<_>>();
        let select_sums = cols
            .iter()
            .map(|c| format!("SUM({c})"))
            .collect::<Vec<_>>()
            .join(", ");
        let insert_cols = cols.join(", ");
        let updates = cols
            .iter()
            .map(|c| format!("{c} = excluded.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "INSERT INTO {dst} (ts, source, {insert_cols})
             SELECT ?1, source, {select_sums}
             FROM {src}
             WHERE ts >= ?1 AND ts < ?2
             GROUP BY source
             ON CONFLICT(ts, source) DO UPDATE SET {updates}",
            dst = table_name(dst),
            src = table_name(src),
        );
        let conn = self.pool.get().expect("get db connection from pool");
        conn.execute(&sql, (start, end))?;
        drop(conn);
        Ok(())
    }

    /// Delete source rows older than `ts` from the `granularity` table.
    ///
    /// # Errors
    /// Returns error if the `SQLite` delete fails.
    ///
    /// # Panics
    /// Panics if the pool cannot hand out a connection.
    pub fn prune_source_before(
        &self,
        granularity: BucketTable,
        ts: i64,
    ) -> Result<usize, ServeError> {
        let conn = self.pool.get().expect("get db connection from pool");
        let sql = format!("DELETE FROM {} WHERE ts < ?1", table_name(granularity));
        let n = conn.execute(&sql, [ts])?;
        drop(conn);
        Ok(n)
    }

    /// Read summary + timeseries together on one connection inside a single read
    /// transaction, so a concurrent writer flush can't land between the two and
    /// skew them against each other within one response.
    ///
    /// # Errors
    /// Returns error if the `SQLite` query fails.
    pub fn source_latency(
        &self,
        table: BucketTable,
        since_ts: i64,
    ) -> Result<(Vec<SourceTotals>, Vec<SourceTsRow>), ServeError> {
        self.with_conn(|conn| {
            let tx = conn.transaction()?;
            let summary = query_source_summary(&tx, table, since_ts)?;
            let timeseries = query_source_timeseries(&tx, table, since_ts)?;
            Ok((summary, timeseries))
        })
    }
}

fn query_source_summary(
    conn: &rusqlite::Connection,
    table: BucketTable,
    since_ts: i64,
) -> Result<Vec<SourceTotals>, ServeError> {
    let ttfb = (0..N_BUCKETS)
        .map(|i| format!("COALESCE(SUM(ttfb{i}),0)"))
        .collect::<Vec<_>>()
        .join(", ");
    let total = (0..N_BUCKETS)
        .map(|i| format!("COALESCE(SUM(total{i}),0)"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT source, COALESCE(SUM(requests),0), COALESCE(SUM(not_modified),0), {ttfb}, {total}
         FROM {tbl} WHERE ts >= ?1 GROUP BY source",
        tbl = table_name(table)
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([since_ts], |row| {
            let g = |i: usize| -> rusqlite::Result<u64> {
                Ok(u64::try_from(row.get::<_, i64>(i)?).unwrap_or(0))
            };
            let mut ttfb = [0u64; N_BUCKETS];
            let mut total = [0u64; N_BUCKETS];
            for k in 0..N_BUCKETS {
                ttfb[k] = g(3 + k)?;
                total[k] = g(3 + N_BUCKETS + k)?;
            }
            Ok(SourceTotals {
                source: row.get(0)?,
                requests: row.get(1)?,
                not_modified: row.get(2)?,
                ttfb,
                total,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(ServeError::from)?;
    Ok(rows)
}

fn query_source_timeseries(
    conn: &rusqlite::Connection,
    table: BucketTable,
    since_ts: i64,
) -> Result<Vec<SourceTsRow>, ServeError> {
    let ttfb = (0..N_BUCKETS)
        .map(|i| format!("COALESCE(SUM(ttfb{i}),0)"))
        .collect::<Vec<_>>()
        .join(", ");
    let total = (0..N_BUCKETS)
        .map(|i| format!("COALESCE(SUM(total{i}),0)"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT source, ts, {ttfb}, {total}
         FROM {tbl} WHERE ts >= ?1 GROUP BY source, ts ORDER BY source, ts",
        tbl = table_name(table)
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([since_ts], |row| {
            let g = |i: usize| -> rusqlite::Result<u64> {
                Ok(u64::try_from(row.get::<_, i64>(i)?).unwrap_or(0))
            };
            let mut ttfb = [0u64; N_BUCKETS];
            let mut total = [0u64; N_BUCKETS];
            for k in 0..N_BUCKETS {
                ttfb[k] = g(2 + k)?;
                total[k] = g(2 + N_BUCKETS + k)?;
            }
            Ok(SourceTsRow {
                source: row.get(0)?,
                ts: row.get(1)?,
                ttfb,
                total,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(ServeError::from)?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::SourceRow;
    use crate::stats::latency::N_BUCKETS;
    use crate::stats::store::Store;
    use tempfile::tempdir;

    fn row(
        ts: i64,
        source: &str,
        requests: u64,
        nm: u64,
        ttfb_i: usize,
        total_i: usize,
    ) -> SourceRow {
        let mut ttfb = [0u64; N_BUCKETS];
        let mut total = [0u64; N_BUCKETS];
        ttfb[ttfb_i] = requests;
        total[total_i] = requests;
        SourceRow {
            ts,
            source: source.into(),
            requests,
            not_modified: nm,
            ttfb,
            total,
        }
    }

    #[test]
    fn upsert_source_accumulates_on_conflict() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.db")).unwrap();
        store
            .upsert_source(&[row(60, "local", 3, 1, 2, 4)])
            .unwrap();
        store
            .upsert_source(&[row(60, "local", 2, 1, 2, 4)])
            .unwrap();
        let conn = store.conn_for_test();
        let (req, nm, ttfb2, total4): (i64, i64, i64, i64) = conn
            .query_row(
                "SELECT requests, not_modified, ttfb2, total4 FROM source_minute WHERE ts=60 AND source='local'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!((req, nm, ttfb2, total4), (5, 2, 5, 5));
    }

    #[test]
    fn rollup_source_sums_and_is_idempotent() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.db")).unwrap();
        store
            .upsert_source(&[
                row(3600, "local", 1, 0, 1, 1),
                row(3660, "local", 1, 0, 1, 1),
            ])
            .unwrap();
        store
            .rollup_source(
                crate::stats::store::BucketTable::Minute,
                crate::stats::store::BucketTable::Hour,
                3600,
                7200,
            )
            .unwrap();
        store
            .rollup_source(
                crate::stats::store::BucketTable::Minute,
                crate::stats::store::BucketTable::Hour,
                3600,
                7200,
            )
            .unwrap();
        let conn = store.conn_for_test();
        let ttfb1: i64 = conn
            .query_row(
                "SELECT ttfb1 FROM source_hour WHERE ts=3600 AND source='local'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ttfb1, 2, "idempotent overwrite, not add");
    }

    #[test]
    fn prune_source_removes_old_rows() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.db")).unwrap();
        store
            .upsert_source(&[
                row(60, "local", 1, 0, 0, 0),
                row(1_000_000, "local", 1, 0, 0, 0),
            ])
            .unwrap();
        let n = store
            .prune_source_before(crate::stats::store::BucketTable::Minute, 1000)
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn source_summary_and_timeseries() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("s.db")).unwrap();
        store
            .upsert_source(&[
                row(60, "local", 4, 2, 1, 1),
                row(120, "proxy:/api", 1, 0, 6, 7),
            ])
            .unwrap();

        let (sum, ts) = store
            .source_latency(crate::stats::store::BucketTable::Minute, 0)
            .unwrap();
        let local = sum.iter().find(|s| s.source == "local").unwrap();
        assert_eq!(local.requests, 4);
        assert_eq!(local.not_modified, 2);
        assert_eq!(local.ttfb[1], 4);

        assert!(
            ts.iter().any(|r| r.source == "proxy:/api"
                && r.ts == 120
                && r.ttfb[6] == 1
                && r.total[7] == 1)
        );
    }
}
