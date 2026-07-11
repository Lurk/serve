pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> i64;
}

#[derive(Clone, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    }
}

/// Monotonic millisecond source, used to measure request latency.
///
/// Separate from `Clock` (wall-clock seconds) because durations need a monotonic
/// base and a finer unit, and because tests must script elapsed values
/// deterministically.
pub trait MonoClock: Send + Sync + 'static {
    fn now_ms(&self) -> u64;
}

pub struct SystemMonoClock {
    base: std::time::Instant,
}

impl SystemMonoClock {
    #[must_use]
    pub fn new() -> Self {
        Self {
            base: std::time::Instant::now(),
        }
    }
}

impl Default for SystemMonoClock {
    fn default() -> Self {
        Self::new()
    }
}

impl MonoClock for SystemMonoClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.base.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
mod mock {
    use super::{Clock, MonoClock};
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicI64, Ordering};

    #[derive(Clone, Default)]
    pub struct MockClock {
        inner: Arc<AtomicI64>,
    }

    impl MockClock {
        #[must_use]
        pub fn new(initial: i64) -> Self {
            Self {
                inner: Arc::new(AtomicI64::new(initial)),
            }
        }
        pub fn set(&self, t: i64) {
            self.inner.store(t, Ordering::SeqCst);
        }
        pub fn advance(&self, delta_seconds: i64) {
            self.inner.fetch_add(delta_seconds, Ordering::SeqCst);
        }
    }

    impl Clock for MockClock {
        fn now(&self) -> i64 {
            self.inner.load(Ordering::SeqCst)
        }
    }

    pub struct MockMono {
        values: Mutex<VecDeque<u64>>,
        last: AtomicI64,
    }

    impl MockMono {
        #[must_use]
        pub fn new(values: Vec<u64>) -> Self {
            Self {
                values: Mutex::new(values.into_iter().collect()),
                last: AtomicI64::new(0),
            }
        }
    }

    impl MonoClock for MockMono {
        fn now_ms(&self) -> u64 {
            let mut q = self.values.lock().expect("mock mono lock");
            #[allow(clippy::option_if_let_else)]
            if let Some(v) = q.pop_front() {
                self.last
                    .store(i64::try_from(v).unwrap_or(i64::MAX), Ordering::SeqCst);
                v
            } else {
                u64::try_from(self.last.load(Ordering::SeqCst)).unwrap_or(0)
            }
        }
    }
}

#[cfg(test)]
pub use mock::{MockClock, MockMono};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_clock_advance() {
        let c = MockClock::new(1000);
        assert_eq!(c.now(), 1000);
        c.advance(60);
        assert_eq!(c.now(), 1060);
        c.set(2000);
        assert_eq!(c.now(), 2000);
    }

    #[test]
    fn mock_mono_returns_scripted_values_in_order() {
        let m = MockMono::new(vec![100, 110, 150]);
        assert_eq!(m.now_ms(), 100);
        assert_eq!(m.now_ms(), 110);
        assert_eq!(m.now_ms(), 150);
        // Past the end, it repeats the last value (defensive: extra body polls).
        assert_eq!(m.now_ms(), 150);
    }

    #[test]
    fn system_mono_is_monotonic_nondecreasing() {
        let c = SystemMonoClock::new();
        let a = c.now_ms();
        let b = c.now_ms();
        assert!(b >= a);
    }
}
