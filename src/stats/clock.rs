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

#[cfg(test)]
mod mock {
    use super::Clock;
    use std::sync::Arc;
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
}

#[cfg(test)]
pub use mock::MockClock;

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
}
