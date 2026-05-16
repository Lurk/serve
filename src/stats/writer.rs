use crate::errors::ServeError;
use crate::stats::clock::Clock;
use crate::stats::recorder::StatEvent;
use crate::stats::store::Store;
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
type BucketAgg = (u64, u64); // requests, bytes

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
        }
    }

    fn ingest(&mut self, ev: StatEvent) {
        let key = (ev.minute_ts, ev.path, ev.status_class);
        let entry = self.map.entry(key).or_insert((0, 0));
        entry.0 = entry.0.saturating_add(1);
        entry.1 = entry.1.saturating_add(ev.bytes);
    }

    fn drain_into_rows(
        map: &mut HashMap<BucketKey, BucketAgg>,
    ) -> Vec<(i64, SmolStr, u8, u64, u64)> {
        map.drain()
            .map(|((ts, path, sc), (r, b))| (ts, path, sc, r, b))
            .collect()
    }

    /// Try to flush the in-memory map. On error, restore the map so events
    /// are retained for the next attempt.
    fn flush(&mut self) -> Result<(), ServeError> {
        if self.map.is_empty() {
            self.handle
                .inner
                .last_flush_ts
                .store(self.clock.now(), Ordering::Relaxed);
            return Ok(());
        }
        let rows = Self::drain_into_rows(&mut self.map);
        if let Err(e) = self.store.upsert_bucket_minute(&rows) {
            self.handle
                .inner
                .write_failures
                .fetch_add(1, Ordering::Relaxed);
            // Restore for next attempt.
            for (ts, path, sc, r, b) in rows {
                self.map.insert((ts, path, sc), (r, b));
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
                        if self.map.len() >= FLUSH_KEY_THRESHOLD {
                            if let Err(e) = self.flush() {
                                tracing::warn!(target: "serve::stats::writer", "size-flush failed: {e}");
                            }
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
        assert_eq!(t.map[&(60, "/a".into(), 2)], (2, 300));
        assert_eq!(t.map[&(60, "/b".into(), 2)], (1, 50));
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
