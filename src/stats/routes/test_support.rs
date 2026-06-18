#![cfg(test)]

use super::StatsState;
use super::login::{get_login, post_login, post_logout};
use super::setup::{get_setup, post_setup};
use crate::stats::auth::{AuthMetrics, SetupTokenState};
use crate::stats::clock::{Clock, MockClock};
use crate::stats::recorder::RecorderHandle;
use crate::stats::store::Store;
use crate::stats::writer::WriterHandle;
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use axum::response::Response;
use axum::routing::{get, post};
use std::sync::Arc;
use tempfile::tempdir;
use tokio::sync::mpsc;

pub(super) fn test_state(setup_required: bool) -> StatsState {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("s.db")).unwrap());
    // Keep the tempdir alive past this function so the SQLite file lives for the test.
    std::mem::forget(dir);
    let clock: Arc<dyn Clock> = Arc::new(MockClock::new(1_700_000_000));
    let (tx, _rx) = mpsc::channel(16);
    let url_prefix: Arc<str> = Arc::from("/__stats__");
    StatsState {
        store,
        clock: clock.clone(),
        metrics: Arc::new(AuthMetrics::default()),
        setup_token: if setup_required {
            SetupTokenState::new_initialised()
        } else {
            SetupTokenState::empty()
        },
        recorder: RecorderHandle::new(tx, clock, url_prefix.clone()),
        writer: WriterHandle::new(),
        session_ttl_days: 30,
        secure_cookies: false,
        geo_enabled: false,
        url_prefix,
    }
}

pub(super) fn setup_app(state: StatsState) -> Router {
    Router::new()
        .route("/__stats__/setup", get(get_setup).post(post_setup))
        .with_state(state)
}

pub(super) fn login_app(state: StatsState) -> Router {
    Router::new()
        .route("/__stats__/login", get(get_login).post(post_login))
        .route("/__stats__/logout", post(post_logout))
        .with_state(state)
}

pub(super) fn full_app(state: StatsState) -> Router {
    super::router(state)
}

pub(super) async fn body_string(resp: Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

pub(super) fn authed(cookie: &str) -> Request<Body> {
    Request::builder()
        .uri("/__stats__")
        .header("cookie", cookie)
        .body(Body::empty())
        .unwrap()
}
