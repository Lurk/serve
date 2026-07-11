use crate::errors::ServeError;
use crate::stats::clock::Clock;
use crate::stats::recorder::StatEvent;
use crate::stats::store::{Dimension, Store};
use smol_str::SmolStr;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const FLUSH_INTERVAL: Duration = Duration::from_secs(10);
pub const FLUSH_KEY_THRESHOLD: usize = 10_000;

type BucketKey = (i64, SmolStr, u8);

#[derive(Default, Clone)]
struct BucketAgg {
    requests: u64,
    bytes: u64,
}

#[derive(Clone)]
struct SourceAgg {
    requests: u64,
    not_modified: u64,
    ttfb: [u64; crate::stats::latency::N_BUCKETS],
    total: [u64; crate::stats::latency::N_BUCKETS],
}

impl Default for SourceAgg {
    fn default() -> Self {
        Self {
            requests: 0,
            not_modified: 0,
            ttfb: [0; crate::stats::latency::N_BUCKETS],
            total: [0; crate::stats::latency::N_BUCKETS],
        }
    }
}

struct WriterMetrics {
    write_failures: AtomicU64,
    // sentinel i64::MIN = "never flushed"; lets last_flush_ts() return Option<i64>.
    last_flush_ts: AtomicI64,
}

#[derive(Clone)]
pub struct WriterHandle {
    inner: Arc<WriterMetrics>,
}

impl Default for WriterHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl WriterHandle {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(WriterMetrics {
                write_failures: AtomicU64::new(0),
                last_flush_ts: AtomicI64::new(i64::MIN),
            }),
        }
    }

    #[must_use]
    pub fn write_failures(&self) -> u64 {
        self.inner.write_failures.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn last_flush_ts(&self) -> Option<i64> {
        let v = self.inner.last_flush_ts.load(Ordering::Relaxed);
        if v == i64::MIN { None } else { Some(v) }
    }
}

pub struct WriterTask {
    rx: mpsc::Receiver<StatEvent>,
    store: Arc<Store>,
    clock: Arc<dyn Clock>,
    handle: WriterHandle,
    shutdown: CancellationToken,
    map: HashMap<BucketKey, BucketAgg>,
    country_map: HashMap<BucketKey, BucketAgg>,
    source_map: HashMap<(i64, SmolStr), SourceAgg>,
}

impl WriterTask {
    pub fn new(
        rx: mpsc::Receiver<StatEvent>,
        store: Arc<Store>,
        clock: Arc<dyn Clock>,
        handle: WriterHandle,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            rx,
            store,
            clock,
            handle,
            shutdown,
            map: HashMap::new(),
            country_map: HashMap::new(),
            source_map: HashMap::new(),
        }
    }

    fn ingest(&mut self, ev: StatEvent) {
        let StatEvent {
            minute_ts,
            path,
            status_class,
            bytes,
            country,
            ttfb_ms,
            total_ms,
            not_modified,
            source,
        } = ev;
        let accumulate = |agg: &mut BucketAgg| {
            agg.requests = agg.requests.saturating_add(1);
            agg.bytes = agg.bytes.saturating_add(bytes);
        };
        accumulate(self.map.entry((minute_ts, path, status_class)).or_default());
        if let Some(cc) = country {
            accumulate(
                self.country_map
                    .entry((minute_ts, cc, status_class))
                    .or_default(),
            );
        }
        let ttfb_i = crate::stats::latency::bucket_index(u64::from(ttfb_ms));
        let tot_i = crate::stats::latency::bucket_index(u64::from(total_ms));
        let s = self.source_map.entry((minute_ts, source)).or_default();
        s.requests = s.requests.saturating_add(1);
        if not_modified {
            s.not_modified = s.not_modified.saturating_add(1);
        }
        s.ttfb[ttfb_i] = s.ttfb[ttfb_i].saturating_add(1);
        s.total[tot_i] = s.total[tot_i].saturating_add(1);
    }

    fn drain_into_rows(
        map: &mut HashMap<BucketKey, BucketAgg>,
    ) -> Vec<crate::stats::store::MinuteRow> {
        map.drain()
            .map(|((ts, key, sc), a)| crate::stats::store::MinuteRow {
                ts,
                key,
                status_class: sc,
                requests: a.requests,
                bytes: a.bytes,
            })
            .collect()
    }

    fn flush_map(
        store: &Store,
        handle: &WriterHandle,
        dim: Dimension,
        map: &mut HashMap<BucketKey, BucketAgg>,
    ) -> Result<(), ServeError> {
        if map.is_empty() {
            return Ok(());
        }
        let rows = Self::drain_into_rows(map);
        if let Err(e) = store.upsert_minute(dim, &rows) {
            handle.inner.write_failures.fetch_add(1, Ordering::Relaxed);
            for r in rows {
                map.insert(
                    (r.ts, r.key, r.status_class),
                    BucketAgg {
                        requests: r.requests,
                        bytes: r.bytes,
                    },
                );
            }
            return Err(e);
        }
        Ok(())
    }

    /// Try to flush the in-memory maps. The path map is flushed first; if it fails
    /// we return without attempting the country flush (each failing map is restored
    /// for retry). An all-empty flush still stamps `last_flush_ts` as a liveness
    /// signal.
    fn flush(&mut self) -> Result<(), ServeError> {
        Self::flush_map(&self.store, &self.handle, Dimension::Path, &mut self.map)?;
        Self::flush_map(
            &self.store,
            &self.handle,
            Dimension::Country,
            &mut self.country_map,
        )?;
        let source_rows: Vec<crate::stats::store::SourceRow> = self
            .source_map
            .drain()
            .map(|((ts, source), a)| crate::stats::store::SourceRow {
                ts,
                source,
                requests: a.requests,
                not_modified: a.not_modified,
                ttfb: a.ttfb,
                total: a.total,
            })
            .collect();
        if let Err(e) = self.store.upsert_source(&source_rows) {
            self.handle
                .inner
                .write_failures
                .fetch_add(1, Ordering::Relaxed);
            for r in source_rows {
                self.source_map.insert(
                    (r.ts, r.source),
                    SourceAgg {
                        requests: r.requests,
                        not_modified: r.not_modified,
                        ttfb: r.ttfb,
                        total: r.total,
                    },
                );
            }
            return Err(e);
        }
        self.handle
            .inner
            .last_flush_ts
            .store(self.clock.now(), Ordering::Relaxed);
        Ok(())
    }

    pub async fn run(mut self) {
        let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => {
                    while let Ok(ev) = self.rx.try_recv() {
                        self.ingest(ev);
                    }
                    if let Err(e) = self.flush() {
                        tracing::warn!(target: "serve::stats::writer", "final flush failed: {e}");
                    }
                    return;
                }
                _ = ticker.tick() => {
                    if let Err(e) = self.flush() {
                        tracing::warn!(target: "serve::stats::writer", "flush failed: {e}");
                    }
                }
                maybe_ev = self.rx.recv() => {
                    if let Some(ev) = maybe_ev {
                        self.ingest(ev);
                        if self.map.len() + self.country_map.len() >= FLUSH_KEY_THRESHOLD
                            && let Err(e) = self.flush() {
                                tracing::warn!(target: "serve::stats::writer", "size-flush failed: {e}");
                            }
                    } else {
                        // Sender dropped — drain & exit.
                        let _ = self.flush();
                        return;
                    }
                }
            }
        }
    }
}

#[must_use]
pub fn spawn_supervised(
    rx: mpsc::Receiver<StatEvent>,
    store: Arc<Store>,
    clock: Arc<dyn Clock>,
    handle: WriterHandle,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let task = WriterTask::new(rx, store, clock, handle, shutdown);
    tokio::spawn(task.run())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::clock::{MockClock, SystemClock};
    use tempfile::tempdir;

    fn ev(ts: i64, path: &str, sc: u8, bytes: u64) -> StatEvent {
        StatEvent {
            minute_ts: ts,
            path: path.into(),
            status_class: sc,
            bytes,
            country: None,
            ttfb_ms: 0,
            total_ms: 0,
            not_modified: false,
            source: "local".into(),
        }
    }

    fn ev_cc(ts: i64, path: &str, sc: u8, bytes: u64, cc: &str) -> StatEvent {
        StatEvent {
            minute_ts: ts,
            path: path.into(),
            status_class: sc,
            bytes,
            country: Some(cc.into()),
            ttfb_ms: 0,
            total_ms: 0,
            not_modified: false,
            source: "local".into(),
        }
    }

    fn task(
        rx: mpsc::Receiver<StatEvent>,
        store: Arc<Store>,
        shutdown: CancellationToken,
    ) -> WriterTask {
        WriterTask::new(
            rx,
            store,
            Arc::new(SystemClock),
            WriterHandle::new(),
            shutdown,
        )
    }

    #[test]
    fn ingest_collapses_same_key() {
        let dir = tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("s.db")).unwrap());
        let (_tx, rx) = mpsc::channel::<StatEvent>(8);
        let mut t = task(rx, store, CancellationToken::new());
        t.ingest(ev(60, "/a", 2, 100));
        t.ingest(ev(60, "/a", 2, 200));
        t.ingest(ev(60, "/b", 2, 50));
        assert_eq!(t.map.len(), 2);
        let a = &t.map[&(60, "/a".into(), 2)];
        assert_eq!((a.requests, a.bytes), (2, 300));
        let b = &t.map[&(60, "/b".into(), 2)];
        assert_eq!((b.requests, b.bytes), (1, 50));
    }

    #[test]
    fn flush_writes_to_store_and_clears_map() {
        let dir = tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("s.db")).unwrap());
        let (_tx, rx) = mpsc::channel::<StatEvent>(8);
        let mut t = task(rx, store.clone(), CancellationToken::new());
        t.ingest(ev(60, "/a", 2, 100));
        t.ingest(ev(60, "/a", 2, 200));
        t.flush().unwrap();
        assert!(t.map.is_empty());

        let rows = store
            .top_assets(
                crate::stats::store::BucketTable::Minute,
                0,
                crate::stats::store::TopMetric::Bytes,
                30,
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, "/a");
        assert_eq!(rows[0].requests, 2);
        assert_eq!(rows[0].bytes, 300);
    }

    #[tokio::test(start_paused = true)]
    async fn flush_on_timer() {
        let dir = tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("s.db")).unwrap());
        let (tx, rx) = mpsc::channel::<StatEvent>(16);
        let shutdown = CancellationToken::new();
        let task = task(rx, store.clone(), shutdown.clone());
        let handle = tokio::spawn(task.run());

        tx.send(ev(60, "/x", 2, 42)).await.unwrap();
        tokio::time::sleep(Duration::from_secs(11)).await;

        let rows = store
            .top_assets(
                crate::stats::store::BucketTable::Minute,
                0,
                crate::stats::store::TopMetric::Bytes,
                30,
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].bytes, 42);

        shutdown.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_flushes_remaining() {
        let dir = tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("s.db")).unwrap());
        let (tx, rx) = mpsc::channel::<StatEvent>(16);
        let shutdown = CancellationToken::new();
        let task = task(rx, store.clone(), shutdown.clone());
        let handle = tokio::spawn(task.run());

        tx.send(ev(60, "/x", 2, 7)).await.unwrap();
        shutdown.cancel();
        handle.await.unwrap();

        let rows = store
            .top_assets(
                crate::stats::store::BucketTable::Minute,
                0,
                crate::stats::store::TopMetric::Bytes,
                30,
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].bytes, 7);
    }

    #[tokio::test]
    async fn spawn_supervised_processes_events() {
        let dir = tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("s.db")).unwrap());
        let (tx, rx) = mpsc::channel::<StatEvent>(8);
        let shutdown = CancellationToken::new();
        let h = spawn_supervised(
            rx,
            store.clone(),
            Arc::new(SystemClock),
            WriterHandle::new(),
            shutdown.clone(),
        );

        tx.send(ev(60, "/x", 2, 9)).await.unwrap();
        shutdown.cancel();
        h.await.unwrap();

        let rows = store
            .top_assets(
                crate::stats::store::BucketTable::Minute,
                0,
                crate::stats::store::TopMetric::Bytes,
                30,
            )
            .unwrap();
        assert_eq!(rows[0].bytes, 9);
    }

    #[test]
    fn handle_starts_with_no_flush_and_zero_failures() {
        let h = WriterHandle::new();
        assert_eq!(h.write_failures(), 0);
        assert_eq!(h.last_flush_ts(), None);
    }

    #[test]
    fn flush_success_stamps_last_flush_ts() {
        let dir = tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("s.db")).unwrap());
        let clock = MockClock::new(1_700_000_000);
        let handle = WriterHandle::new();
        let (_tx, rx) = mpsc::channel::<StatEvent>(8);
        let mut t = WriterTask::new(
            rx,
            store,
            Arc::new(clock.clone()),
            handle.clone(),
            CancellationToken::new(),
        );

        t.ingest(ev(60, "/a", 2, 100));
        t.flush().unwrap();
        assert_eq!(handle.last_flush_ts(), Some(1_700_000_000));
        assert_eq!(handle.write_failures(), 0);

        // Empty-map flush also stamps last_flush_ts — it's a liveness signal,
        // not a "wrote rows" signal.
        clock.set(1_700_000_010);
        t.flush().unwrap();
        assert_eq!(handle.last_flush_ts(), Some(1_700_000_010));
    }

    #[test]
    fn flush_writes_country_rows() {
        let dir = tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("s.db")).unwrap());
        let (_tx, rx) = mpsc::channel::<StatEvent>(8);
        let mut t = task(rx, store.clone(), CancellationToken::new());
        t.ingest(ev_cc(60, "/a", 2, 100, "US"));
        t.ingest(ev_cc(60, "/b", 2, 200, "US"));
        t.ingest(ev_cc(60, "/c", 4, 5, "DE"));
        t.flush().unwrap();

        let rows = store
            .country_breakdown(crate::stats::store::BucketTable::Minute, 0)
            .unwrap();
        let us = rows.iter().find(|r| r.country == "US").unwrap();
        assert_eq!(us.requests, 2);
        assert_eq!(us.bytes, 300);
        let de = rows.iter().find(|r| r.country == "DE").unwrap();
        assert_eq!(de.status_class, 4);
        assert_eq!(de.requests, 1);
        // Path buckets still populated as before.
        let assets = store
            .top_assets(
                crate::stats::store::BucketTable::Minute,
                0,
                crate::stats::store::TopMetric::Bytes,
                30,
            )
            .unwrap();
        assert_eq!(assets.len(), 2); // /a and /b are 2xx; /c is 4xx (excluded by top_assets)
    }

    #[test]
    fn ingest_bins_source_latency_and_304() {
        let dir = tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("s.db")).unwrap());
        let (_tx, rx) = mpsc::channel::<StatEvent>(8);
        let mut t = task(rx, store.clone(), CancellationToken::new());
        // ttfb 3ms -> bucket 1 ([1,5)); total 30ms -> bucket 4 ([25,50)); 304; source local.
        let mut ev = ev(60, "/a", 3, 0);
        ev.ttfb_ms = 3;
        ev.total_ms = 30;
        ev.not_modified = true;
        let ev2 = ev.clone();
        t.ingest(ev);
        t.ingest(ev2);
        t.flush().unwrap();
        let conn = store.conn_for_test();
        let (ttfb1, tot4, nm): (i64, i64, i64) = conn
            .query_row(
                "SELECT ttfb1, total4, not_modified FROM source_minute WHERE ts=60 AND source='local'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((ttfb1, tot4, nm), (2, 2, 2));
    }

    #[test]
    fn flush_failure_increments_counter_and_does_not_stamp_ts() {
        let dir = tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("s.db")).unwrap());
        let handle = WriterHandle::new();
        let (_tx, rx) = mpsc::channel::<StatEvent>(8);
        let mut t = WriterTask::new(
            rx,
            store.clone(),
            Arc::new(MockClock::new(1_700_000_000)),
            handle.clone(),
            CancellationToken::new(),
        );

        store.drop_bucket_minute_for_test();
        t.ingest(ev(60, "/a", 2, 100));
        assert!(t.flush().is_err());
        assert_eq!(handle.write_failures(), 1);
        assert_eq!(handle.last_flush_ts(), None);
        // Events are restored so a later successful flush can drain them.
        assert_eq!(t.map.len(), 1);
    }
}
