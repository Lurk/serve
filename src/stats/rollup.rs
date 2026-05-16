use crate::errors::ServeError;
use crate::stats::clock::Clock;
use crate::stats::store::Store;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub const RETENTION_MINUTE_SECS: i64 = 25 * 3600; // 25 hours
pub const RETENTION_HOUR_SECS: i64 = 32 * 86_400; // 32 days
pub const RETENTION_DAY_SECS: i64 = 400 * 86_400; // 400 days

pub struct RollupTask {
    store: Arc<Store>,
    clock: Arc<dyn Clock>,
    shutdown: CancellationToken,
}

impl RollupTask {
    pub fn new(store: Arc<Store>, clock: Arc<dyn Clock>, shutdown: CancellationToken) -> Self {
        Self {
            store,
            clock,
            shutdown,
        }
    }

    /// Run one rollup cycle: aggregate hour/day buckets and prune expired rows.
    ///
    /// # Errors
    /// Returns error if any store operation during the rollup fails.
    pub fn tick(&self) -> Result<(), ServeError> {
        let now = self.clock.now();
        self.do_hour_rollups(now)?;
        self.do_day_rollups(now)?;
        self.store
            .prune_bucket_minute_before(now - RETENTION_MINUTE_SECS)?;
        self.store
            .prune_bucket_hour_before(now - RETENTION_HOUR_SECS)?;
        self.store
            .prune_bucket_day_before(now - RETENTION_DAY_SECS)?;
        self.store.prune_expired_sessions(now)?;
        Ok(())
    }

    fn do_hour_rollups(&self, now: i64) -> Result<(), ServeError> {
        let last_complete_hour = (now / 3600 - 1) * 3600;
        let last = self.resolve_rollup_start(
            "last_hour_rollup_ts",
            crate::stats::store::BucketTable::Minute,
            3600,
            last_complete_hour,
        )?;
        if last_complete_hour <= last {
            return Ok(());
        }
        let start = last + 3600;
        let mut h = start;
        while h <= last_complete_hour {
            self.store.rollup_hour(h, h + 3600)?;
            self.store.meta_set("last_hour_rollup_ts", &h.to_string())?;
            h += 3600;
        }
        Ok(())
    }

    fn do_day_rollups(&self, now: i64) -> Result<(), ServeError> {
        let last_complete_day = (now / 86_400 - 1) * 86_400;
        let last = self.resolve_rollup_start(
            "last_day_rollup_ts",
            crate::stats::store::BucketTable::Hour,
            86_400,
            last_complete_day,
        )?;
        if last_complete_day <= last {
            return Ok(());
        }
        let start = last + 86_400;
        let mut d = start;
        while d <= last_complete_day {
            self.store.rollup_day(d, d + 86_400)?;
            self.store.meta_set("last_day_rollup_ts", &d.to_string())?;
            d += 86_400;
        }
        Ok(())
    }

    /// Resolve where the next rollup should begin. When `meta_key` is unset
    /// (fresh DB), seed from the earliest source-table row so we never iterate
    /// from epoch — otherwise the first tick on a brand-new install would do
    /// ~485k DB writes spanning 1970→now and refuse to honor cancellation.
    fn resolve_rollup_start(
        &self,
        meta_key: &str,
        source: crate::stats::store::BucketTable,
        bucket_secs: i64,
        last_complete_bucket: i64,
    ) -> Result<i64, ServeError> {
        if let Some(s) = self.store.meta_get(meta_key)? {
            match s.parse::<i64>() {
                Ok(v) => return Ok(v),
                Err(e) => {
                    // A corrupted meta value would otherwise fall back to 0
                    // and trigger ~485k rollup iterations from 1970.
                    tracing::warn!(
                        target: "serve::stats::rollup",
                        "meta[{meta_key}]={s:?} not parseable as i64 ({e}); re-seeding to {last_complete_bucket}"
                    );
                    self.store
                        .meta_set(meta_key, &last_complete_bucket.to_string())?;
                    return Ok(last_complete_bucket);
                }
            }
        }
        let seed = self
            .store
            .min_bucket_ts(source)?
            .map_or(last_complete_bucket, |min_ts| {
                (min_ts / bucket_secs) * bucket_secs - bucket_secs
            });
        self.store.meta_set(meta_key, &seed.to_string())?;
        Ok(seed)
    }

    pub async fn run(self) {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => return,
                _ = ticker.tick() => {
                    if let Err(e) = self.tick() {
                        tracing::warn!(target: "serve::stats::rollup", "tick failed: {e}");
                    }
                }
            }
        }
    }
}

pub fn spawn(
    store: Arc<Store>,
    clock: Arc<dyn Clock>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let task = RollupTask::new(store, clock, shutdown);
    tokio::spawn(task.run())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::clock::MockClock;
    use crate::stats::store::{BucketTable, TopMetric};
    use tempfile::tempdir;

    fn setup() -> (Arc<Store>, Arc<MockClock>, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("s.db")).unwrap());
        let clock = Arc::new(MockClock::new(0));
        (store, clock, dir)
    }

    #[test]
    fn tick_prunes_old_minute_buckets() {
        let (store, clock, _dir) = setup();
        store
            .upsert_bucket_minute(&[
                (60, "/a".into(), 2, 1, 1),
                (1_000_000, "/a".into(), 2, 1, 1),
            ])
            .unwrap();
        clock.set(1_000_000 + 100);
        let shutdown = CancellationToken::new();
        let task = RollupTask::new(store.clone(), clock, shutdown);
        task.tick().unwrap();

        let conn = store.conn_for_test();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM bucket_minute WHERE ts=60", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "old row should be pruned");
        let count2: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM bucket_minute WHERE ts=1000000",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count2, 1, "recent row should remain");
        drop(conn);
    }

    #[test]
    fn tick_runs_hour_rollup_idempotent() {
        let (store, clock, _dir) = setup();
        store
            .upsert_bucket_minute(&[(3600, "/a".into(), 2, 5, 50), (4200, "/a".into(), 2, 2, 20)])
            .unwrap();
        clock.set(7500);
        let shutdown = CancellationToken::new();
        let task = RollupTask::new(store.clone(), clock, shutdown);
        task.tick().unwrap();
        task.tick().unwrap();

        let rows = store
            .top_assets(BucketTable::Hour, 0, TopMetric::Bytes, 30)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].requests, 7);
        assert_eq!(rows[0].bytes, 70);
    }

    #[test]
    fn corrupt_meta_reseeds_to_last_complete_bucket() {
        let (store, clock, _dir) = setup();
        // Operator-tampered (or otherwise non-numeric) meta value.
        store
            .meta_set("last_hour_rollup_ts", "not-a-number")
            .unwrap();
        clock.set(10_000);
        let task = RollupTask::new(store.clone(), clock, CancellationToken::new());
        task.tick().unwrap();

        // After re-seed the value should parse as i64 again, at the expected boundary.
        let v = store.meta_get("last_hour_rollup_ts").unwrap().unwrap();
        let parsed: i64 = v.parse().unwrap();
        // last_complete_hour for now=10_000 is (10000/3600 - 1) * 3600 = 3600.
        assert_eq!(parsed, 3600);
    }

    #[test]
    fn tick_backfills_multiple_hours() {
        let (store, clock, _dir) = setup();
        store
            .upsert_bucket_minute(&[
                (3600, "/a".into(), 2, 1, 1),
                (7200, "/a".into(), 2, 1, 1),
                (10800, "/a".into(), 2, 1, 1),
            ])
            .unwrap();
        clock.set(14_500);
        let task = RollupTask::new(store.clone(), clock, CancellationToken::new());
        task.tick().unwrap();

        let rows = store.timeseries(BucketTable::Hour, 0).unwrap();
        let hours: std::collections::HashSet<i64> = rows.iter().map(|r| r.ts).collect();
        assert!(hours.contains(&3600));
        assert!(hours.contains(&7200));
        assert!(hours.contains(&10800));
    }
}
