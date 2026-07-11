pub mod api;
pub mod dashboard;
pub mod login;
pub mod session;
pub mod setup;

#[cfg(test)]
mod test_support;

use crate::errors::ServeError;
use crate::stats::auth::{AuthMetrics, SetupTokenState};
use crate::stats::clock::Clock;
use crate::stats::recorder::RecorderHandle;
use crate::stats::store::Store;
use crate::stats::writer::WriterHandle;
use std::sync::Arc;

#[derive(Clone)]
pub struct StatsState {
    pub store: Arc<Store>,
    pub clock: Arc<dyn Clock>,
    pub metrics: Arc<AuthMetrics>,
    pub setup_token: SetupTokenState,
    pub recorder: RecorderHandle,
    pub writer: WriterHandle,
    pub session_ttl_days: u32,
    pub secure_cookies: bool,
    /// Whether a `GeoIP` database is loaded — gates the `/api/countries` payload.
    pub geo_enabled: bool,
    /// Validated URL prefix the dashboard mounts under (no trailing slash).
    pub url_prefix: Arc<str>,
}

impl StatsState {
    /// Build the URL the browser should be sent to for `suffix` (which should
    /// start with `/` or be empty).
    #[must_use]
    pub fn url(&self, suffix: &str) -> String {
        format!("{}{}", self.url_prefix, suffix)
    }
}

/// Run a blocking `Store` operation on the tokio blocking pool.
///
/// # Errors
/// Returns `ServeError::Stats` if the blocking task panicked, or whatever
/// error `f` produces.
async fn db<F, R>(store: Arc<Store>, f: F) -> Result<R, ServeError>
where
    F: FnOnce(&Store) -> Result<R, ServeError> + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(move || f(&store))
        .await
        .map_err(|e| ServeError::Stats(format!("blocking task panicked: {e}")))?
}

pub fn router(state: StatsState) -> axum::Router {
    use axum::routing::{get, post};
    let p = state.url_prefix.as_ref();
    axum::Router::new()
        .route(p, get(dashboard::get_dashboard))
        .route(
            &format!("{p}/setup"),
            get(setup::get_setup).post(setup::post_setup),
        )
        .route(
            &format!("{p}/login"),
            get(login::get_login).post(login::post_login),
        )
        .route(&format!("{p}/logout"), post(login::post_logout))
        .route(&format!("{p}/api/timeseries"), get(api::get_timeseries))
        .route(&format!("{p}/api/assets"), get(api::get_assets))
        .route(&format!("{p}/api/countries"), get(api::get_countries))
        .route(&format!("{p}/api/summary"), get(api::get_summary))
        .route(&format!("{p}/api/health"), get(api::get_health))
        .route(&format!("{p}/api/latency"), get(api::get_latency))
        .with_state(state)
        .layer(axum::middleware::from_fn(security_headers))
}

/// Add hardening headers to every stats response: frame-ancestors none
/// (defeats click-jacking of the dashboard), nosniff, same-origin referrer,
/// and a strict CSP. Inline style/script are allowed because the dashboard
/// ships its CSS and JS inside the HTML.
async fn security_headers(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::HeaderValue;
    use axum::http::header::{
        CONTENT_SECURITY_POLICY, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
    };
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    h.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; \
             style-src 'self' 'unsafe-inline'; \
             script-src 'self' 'unsafe-inline'; \
             img-src 'self' data:; \
             frame-ancestors 'none'; \
             base-uri 'none'",
        ),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::test_support::{full_app, test_state};
    use axum::body::Body;
    use axum::http::{Request, header};
    use tower::ServiceExt;

    #[tokio::test]
    async fn stats_responses_carry_security_headers() {
        let app = full_app(test_state(true));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/__stats__/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let h = resp.headers();
        assert_eq!(h.get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
        assert_eq!(h.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
        assert_eq!(h.get(header::REFERRER_POLICY).unwrap(), "same-origin");
        let csp = h
            .get(header::CONTENT_SECURITY_POLICY)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(csp.contains("frame-ancestors 'none'"));
    }
}
