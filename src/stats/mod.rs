pub mod auth;
pub mod clock;
pub mod config;
pub mod geo;
pub mod recorder;
pub mod rollup;
pub mod routes;
pub mod store;
pub mod templates;
pub mod writer;

pub use clock::{Clock, SystemClock};
pub use config::{DEFAULT_URL_PREFIX, StatsConfig};
pub use recorder::{RecorderHandle, StatsRecorderLayer};
pub use routes::StatsState;
pub use writer::WriterHandle;

use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::errors::ServeError;
use auth::{AuthMetrics, SetupTokenState};
use geo::{CountryResolver, GeoResolver};
use recorder::StatEvent;
use store::Store;

pub struct StatsHandle {
    pub recorder: RecorderHandle,
    pub router: axum::Router,
    shutdown: CancellationToken,
    writer: Option<tokio::task::JoinHandle<()>>,
    rollup: Option<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for StatsHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatsHandle").finish_non_exhaustive()
    }
}

impl StatsHandle {
    /// Start the stats subsystem: open the store, spawn the writer and rollup tasks, and build
    /// the stats router.
    ///
    /// # Errors
    /// Returns error if `session_ttl_days` is 0, or if the store fails to open.
    pub async fn start(config: &StatsConfig, tls_subcommand: bool) -> Result<Self, ServeError> {
        if config.session_ttl_days == 0 {
            return Err(ServeError::Stats(
                "session_ttl_days must be >= 1".to_string(),
            ));
        }
        let secure_cookies = config.secure_cookies.unwrap_or(tls_subcommand);
        let prefix_input = config.url_prefix.as_deref().unwrap_or(DEFAULT_URL_PREFIX);
        let url_prefix: Arc<str> =
            Arc::from(config::validate_url_prefix(prefix_input).map_err(ServeError::Stats)?);
        let db_path = config.db_path.clone();
        let store = Arc::new(
            tokio::task::spawn_blocking(move || Store::open(&db_path))
                .await
                .map_err(|e| ServeError::Stats(format!("store-open task panicked: {e}")))??,
        );
        let store_for_lookup = store.clone();
        let has_password = tokio::task::spawn_blocking(move || store_for_lookup.password_hash())
            .await
            .map_err(|e| ServeError::Stats(format!("password_hash task panicked: {e}")))??
            .is_some();
        let setup_token = if has_password {
            SetupTokenState::empty()
        } else {
            let s = SetupTokenState::new_initialised();
            if let Some(tok) = s.read() {
                // stderr only — never write the token to tracing, since --log-path
                // would otherwise persist it to a log file on disk.
                eprintln!("stats setup token: {tok}");
            }
            s
        };
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let geo: Option<Arc<dyn CountryResolver>> =
            config
                .geoip_db_path
                .as_deref()
                .and_then(|path| match GeoResolver::open(path) {
                    Ok(r) => {
                        tracing::info!(
                            target: "serve::stats",
                            "geoip database loaded from {}", path.display()
                        );
                        Some(Arc::new(r) as Arc<dyn CountryResolver>)
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "serve::stats",
                            "geoip database unavailable ({e}); country stats disabled"
                        );
                        None
                    }
                });
        let geo_enabled = geo.is_some();
        let (tx, rx) = mpsc::channel::<StatEvent>(4096);
        let recorder = RecorderHandle::new(tx, clock.clone(), url_prefix.clone())
            .with_geo(geo, config.trust_forwarded_for);
        let writer = WriterHandle::new();
        let metrics = Arc::new(AuthMetrics::default());
        let shutdown = CancellationToken::new();

        let writer_join = writer::spawn_supervised(
            rx,
            store.clone(),
            clock.clone(),
            writer.clone(),
            shutdown.clone(),
        );
        let rollup_handle = rollup::spawn(store.clone(), clock.clone(), shutdown.clone());

        let state = StatsState {
            store,
            clock,
            metrics,
            setup_token,
            recorder: recorder.clone(),
            writer,
            session_ttl_days: config.session_ttl_days,
            secure_cookies,
            geo_enabled,
            url_prefix,
        };

        let router = routes::router(state);
        tracing::info!(target: "serve::stats::recorder", "recorder layer attached, channel cap 4096");

        Ok(Self {
            recorder,
            router,
            shutdown,
            writer: Some(writer_join),
            rollup: Some(rollup_handle),
        })
    }

    #[must_use]
    pub fn recorder_layer(&self) -> StatsRecorderLayer {
        StatsRecorderLayer::new(self.recorder.clone())
    }

    pub async fn shutdown(mut self) {
        self.shutdown.cancel();
        let timeout = std::time::Duration::from_secs(5);
        if let Some(w) = self.writer.take()
            && tokio::time::timeout(timeout, w).await.is_err()
        {
            tracing::warn!(target: "serve::stats", "writer task did not shut down within 5s; aborting");
        }
        if let Some(r) = self.rollup.take()
            && tokio::time::timeout(timeout, r).await.is_err()
        {
            tracing::warn!(target: "serve::stats", "rollup task did not shut down within 5s; aborting");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn start_then_shutdown_on_fresh_db() {
        let dir = tempdir().unwrap();
        let cfg = StatsConfig {
            db_path: dir.path().join("s.db"),
            session_ttl_days: 30,
            secure_cookies: None,
            url_prefix: None,
            geoip_db_path: None,
            trust_forwarded_for: false,
        };
        let h = StatsHandle::start(&cfg, false).await.unwrap();
        h.shutdown().await;
    }

    #[tokio::test]
    async fn start_rejects_zero_ttl() {
        let dir = tempdir().unwrap();
        let cfg = StatsConfig {
            db_path: dir.path().join("s.db"),
            session_ttl_days: 0,
            secure_cookies: None,
            url_prefix: None,
            geoip_db_path: None,
            trust_forwarded_for: false,
        };
        let err = StatsHandle::start(&cfg, false).await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("session_ttl_days"));
    }

    #[tokio::test]
    async fn start_then_restart_keeps_setup_token_cleared() {
        let dir = tempdir().unwrap();
        let cfg = StatsConfig {
            db_path: dir.path().join("s.db"),
            session_ttl_days: 30,
            secure_cookies: None,
            url_prefix: None,
            geoip_db_path: None,
            trust_forwarded_for: false,
        };
        let h1 = StatsHandle::start(&cfg, false).await.unwrap();
        {
            let store = crate::stats::store::Store::open(&cfg.db_path).unwrap();
            store.set_password_hash("$argon2id$placeholder", 0).unwrap();
        }
        h1.shutdown().await;

        let h2 = StatsHandle::start(&cfg, false).await.unwrap();
        h2.shutdown().await;
    }
}
