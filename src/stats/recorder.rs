use axum::http::{Request, Response};
use bytes::Buf;
use http_body::{Body, Frame, SizeHint};
use smol_str::SmolStr;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tower::{Layer, Service};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatEvent {
    pub minute_ts: i64,
    pub path: SmolStr,
    pub status_class: u8,
    pub bytes: u64,
    /// Resolved client country (ISO alpha-2), or `None` when geo is disabled.
    pub country: Option<SmolStr>,
}

pub const MAX_PATH_BYTES: usize = 512;

/// Strip query string from a path-and-query, truncate to `MAX_PATH_BYTES` on
/// a UTF-8 boundary, and append `…` if truncation occurred.
#[must_use]
pub fn canonicalize_path(input: &str) -> SmolStr {
    let no_query = match input.split_once('?') {
        Some((p, _)) => p,
        None => input,
    };
    if no_query.len() <= MAX_PATH_BYTES {
        return SmolStr::new(no_query);
    }
    let mut cut = MAX_PATH_BYTES;
    while cut > 0 && !no_query.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::with_capacity(cut + 3);
    out.push_str(&no_query[..cut]);
    // The dashboard's renderAssets JS detects truncation by checking
    // `path.endsWith('…')`; changing this sentinel requires updating it too.
    out.push('…');
    SmolStr::new(out)
}

#[must_use]
pub const fn status_class(code: u16) -> u8 {
    match code {
        100..=199 => 1,
        200..=299 => 2,
        300..=399 => 3,
        400..=499 => 4,
        _ => 5,
    }
}

#[must_use]
pub const fn minute_floor(ts: i64) -> i64 {
    ts - ts.rem_euclid(60)
}

#[derive(Clone)]
pub struct RecorderHandle {
    tx: mpsc::Sender<StatEvent>,
    dropped: Arc<AtomicU64>,
    clock: Arc<dyn crate::stats::clock::Clock>,
    /// Requests whose path starts with this prefix are not recorded — the
    /// dashboard would otherwise show itself as the busiest asset.
    skip_prefix: Arc<str>,
    /// Resolves client IPs to country codes. `None` disables country capture.
    geo: Option<Arc<dyn crate::stats::geo::CountryResolver>>,
    /// Trust `X-Forwarded-For` for the client IP (see config).
    trust_forwarded_for: bool,
}

impl RecorderHandle {
    #[must_use]
    pub fn new(
        tx: mpsc::Sender<StatEvent>,
        clock: Arc<dyn crate::stats::clock::Clock>,
        skip_prefix: Arc<str>,
    ) -> Self {
        Self {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
            clock,
            skip_prefix,
            geo: None,
            trust_forwarded_for: false,
        }
    }
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Attach a country resolver and the forwarded-for trust flag. Returns
    /// `self` so it chains after `new`.
    #[must_use]
    pub fn with_geo(
        mut self,
        geo: Option<Arc<dyn crate::stats::geo::CountryResolver>>,
        trust_forwarded_for: bool,
    ) -> Self {
        self.geo = geo;
        self.trust_forwarded_for = trust_forwarded_for;
        self
    }

    /// Resolve the request's client country, or `None` when geo is disabled.
    fn resolve_country<B>(&self, req: &Request<B>) -> Option<SmolStr> {
        let geo = self.geo.as_ref()?;
        let ip = self.client_ip(req)?;
        Some(geo.country_code(ip))
    }

    /// Pick the client IP: the right-most `X-Forwarded-For` entry when trusted,
    /// otherwise the socket peer from `ConnectInfo`.
    ///
    /// Proxies *append* the address they received the connection from, so the
    /// right-most entry is the one our own trusted proxy observed — the only
    /// value a client cannot spoof by sending its own `X-Forwarded-For`. We scan
    /// every `x-forwarded-for` header (the value may be split across lines) and
    /// take the *right-most* entry, then parse it. We anchor on that last token
    /// and do NOT skip to an earlier entry when it fails to parse — otherwise a
    /// proxy that appends an unparseable token (e.g. an `ip:port` pair) would
    /// hand selection to a client-controlled entry, defeating the trust model.
    /// On a malformed right-most token we fall back to the socket peer instead.
    /// This assumes exactly one trusted proxy directly in front of `serve`.
    fn client_ip<B>(&self, req: &Request<B>) -> Option<IpAddr> {
        if self.trust_forwarded_for {
            let rightmost = req
                .headers()
                .get_all("x-forwarded-for")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|s| s.split(','))
                .map(str::trim)
                .rfind(|s| !s.is_empty());
            if let Some(token) = rightmost
                && let Some(ip) = parse_forwarded_addr(token)
            {
                return Some(ip);
            }
            // No XFF entry, or the trusted right-most token is unparseable: fall
            // through to the socket peer rather than an earlier, spoofable entry.
        }
        req.extensions()
            .get::<axum::extract::ConnectInfo<SocketAddr>>()
            .map(|ci| ci.0.ip())
    }
}

/// Parse a single `X-Forwarded-For` token, which may be a bare IP (`1.2.3.4`,
/// `2001:db8::1`) or carry a port (`1.2.3.4:443`, `[2001:db8::1]:443`). Returns
/// the address, or `None` if the token is not an IP at all.
fn parse_forwarded_addr(s: &str) -> Option<IpAddr> {
    if let Ok(ip) = s.parse::<IpAddr>() {
        return Some(ip);
    }
    s.parse::<SocketAddr>().ok().map(|sa| sa.ip())
}

#[derive(Clone)]
pub struct StatsRecorderLayer {
    handle: RecorderHandle,
}

impl StatsRecorderLayer {
    #[must_use]
    pub const fn new(handle: RecorderHandle) -> Self {
        Self { handle }
    }
}

impl<S> Layer<S> for StatsRecorderLayer {
    type Service = StatsRecorder<S>;
    fn layer(&self, inner: S) -> Self::Service {
        StatsRecorder {
            inner,
            handle: self.handle.clone(),
        }
    }
}

#[derive(Clone)]
pub struct StatsRecorder<S> {
    inner: S,
    handle: RecorderHandle,
}

/// Counts bytes streamed out of a response body.
///
/// The callback fires once — either when the inner body signals end-of-stream,
/// or when the wrapper is dropped (covering client disconnect mid-response).
pub struct CountingBody<B> {
    inner: B,
    bytes: u64,
    on_done: Option<Box<dyn FnOnce(u64) + Send>>,
}

impl<B> CountingBody<B> {
    fn new<F>(inner: B, on_done: F) -> Self
    where
        F: FnOnce(u64) + Send + 'static,
    {
        Self {
            inner,
            bytes: 0,
            on_done: Some(Box::new(on_done)),
        }
    }
}

impl<B> Drop for CountingBody<B> {
    fn drop(&mut self) {
        if let Some(cb) = self.on_done.take() {
            cb(self.bytes);
        }
    }
}

impl<B> Body for CountingBody<B>
where
    B: Body + Unpin,
    B::Data: Buf,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        match polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    self.bytes = self
                        .bytes
                        .saturating_add(u64::try_from(data.remaining()).unwrap_or(u64::MAX));
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(None) => {
                if let Some(cb) = self.on_done.take() {
                    cb(self.bytes);
                }
                Poll::Ready(None)
            }
            other => other,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for StatsRecorder<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    ReqBody: Send + 'static,
    ResBody: Body + Send + Unpin + 'static,
    ResBody::Data: Buf,
{
    type Response = Response<CountingBody<ResBody>>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        let path_and_query = req.uri().path_and_query().map_or_else(
            || req.uri().path().to_string(),
            std::string::ToString::to_string,
        );
        let skip = req
            .uri()
            .path()
            .starts_with(self.handle.skip_prefix.as_ref());
        let country = if skip {
            None
        } else {
            self.handle.resolve_country(&req)
        };
        let handle = self.handle.clone();
        let fut = self.inner.call(req);

        Box::pin(async move {
            let response = fut.await?;
            let on_done: Box<dyn FnOnce(u64) + Send> = if skip {
                Box::new(|_| {})
            } else {
                let path = canonicalize_path(&path_and_query);
                let minute_ts = minute_floor(handle.clock.now());
                let status_class = status_class(response.status().as_u16());
                Box::new(move |bytes| {
                    let event = StatEvent {
                        minute_ts,
                        path,
                        status_class,
                        bytes,
                        country,
                    };
                    match handle.tx.try_send(event) {
                        Ok(()) => {}
                        Err(
                            mpsc::error::TrySendError::Full(_)
                            | mpsc::error::TrySendError::Closed(_),
                        ) => {
                            handle.dropped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
            };
            let (parts, body) = response.into_parts();
            Ok(Response::from_parts(
                parts,
                CountingBody::new(body, on_done),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_strips_query() {
        assert_eq!(canonicalize_path("/foo.js?v=2"), "/foo.js");
        assert_eq!(canonicalize_path("/x?a=1&b=2"), "/x");
        assert_eq!(canonicalize_path("/x"), "/x");
        assert_eq!(canonicalize_path("/?just-query"), "/");
    }

    #[test]
    fn path_truncates_at_512_bytes_with_ellipsis() {
        let long = format!("/{}", "a".repeat(600));
        let out = canonicalize_path(&long);
        assert!(out.ends_with('…'));
        let core_len = out.len() - '…'.len_utf8();
        assert!(core_len <= MAX_PATH_BYTES);
    }

    #[test]
    fn path_truncation_does_not_split_codepoint() {
        let mut s = String::from("/");
        while s.len() < 600 {
            s.push('🦀');
        }
        let out = canonicalize_path(&s);
        let body = out.trim_end_matches('…');
        assert!(
            body.chars().all(|c| c == '/' || c == '🦀'),
            "truncated body contains a split codepoint"
        );
    }

    #[test]
    fn status_class_mapping() {
        assert_eq!(status_class(200), 2);
        assert_eq!(status_class(204), 2);
        assert_eq!(status_class(301), 3);
        assert_eq!(status_class(404), 4);
        assert_eq!(status_class(500), 5);
        assert_eq!(status_class(599), 5);
        assert_eq!(status_class(100), 1);
    }

    #[test]
    fn minute_floor_truncates() {
        assert_eq!(minute_floor(123), 120);
        assert_eq!(minute_floor(60), 60);
        assert_eq!(minute_floor(0), 0);
        assert_eq!(minute_floor(3599), 3540);
        assert_eq!(minute_floor(-1), -60);
        assert_eq!(minute_floor(-60), -60);
        assert_eq!(minute_floor(-61), -120);
    }

    use crate::stats::clock::MockClock;
    use crate::stats::geo::CountryResolver;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::response::Redirect;
    use std::net::{IpAddr, SocketAddr};
    use std::sync::Arc;
    use tower::ServiceExt;

    fn echo_router() -> Router {
        Router::new()
            .route("/200", axum::routing::get(|| async { "ok" }))
            .route("/big", axum::routing::get(|| async { "ok".repeat(2000) }))
            .route(
                "/404path",
                axum::routing::get(|| async { (StatusCode::NOT_FOUND, "nope") }),
            )
            .route(
                "/500path",
                axum::routing::get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom!") }),
            )
            .route(
                "/302path",
                axum::routing::get(|| async { Redirect::to("/elsewhere") }),
            )
            .route(
                "/__stats__/something",
                axum::routing::get(|| async { "stats" }),
            )
    }

    async fn drain(resp: axum::http::Response<Body>) -> usize {
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .len()
    }

    #[tokio::test]
    async fn records_2xx_with_actual_body_bytes() {
        let clock = Arc::new(MockClock::new(1_700_000_120));
        let (tx, mut rx) = mpsc::channel::<StatEvent>(16);
        let handle = RecorderHandle::new(tx, clock, Arc::from("/__stats__"));
        let app = echo_router().layer(StatsRecorderLayer::new(handle));

        let req = Request::builder()
            .uri("/big?v=1")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let len = drain(resp).await;
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.path.as_str(), "/big");
        assert_eq!(ev.status_class, 2);
        assert_eq!(ev.bytes, len as u64);
        assert_eq!(ev.bytes, 4000);
        assert_eq!(ev.minute_ts, 1_700_000_100);
    }

    #[tokio::test]
    async fn records_status_class_per_response() {
        for (uri, expected_class, expected_bytes) in [
            ("/302path", 3u8, None::<u64>),
            ("/404path", 4u8, Some("nope".len() as u64)),
            ("/500path", 5u8, Some("boom!".len() as u64)),
        ] {
            let clock = Arc::new(MockClock::new(0));
            let (tx, mut rx) = mpsc::channel::<StatEvent>(16);
            let handle = RecorderHandle::new(tx, clock, Arc::from("/__stats__"));
            let app = echo_router().layer(StatsRecorderLayer::new(handle));

            let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
            let resp = app.oneshot(req).await.unwrap();
            let len = drain(resp).await;
            let ev = rx.recv().await.unwrap();
            assert_eq!(ev.status_class, expected_class, "uri {uri}");
            assert_eq!(ev.bytes, len as u64, "uri {uri}");
            if let Some(b) = expected_bytes {
                assert_eq!(ev.bytes, b, "uri {uri}");
            }
        }
    }

    #[tokio::test]
    async fn records_bytes_on_dropped_body() {
        let clock = Arc::new(MockClock::new(0));
        let (tx, mut rx) = mpsc::channel::<StatEvent>(16);
        let handle = RecorderHandle::new(tx, clock, Arc::from("/__stats__"));
        let app = echo_router().layer(StatsRecorderLayer::new(handle));

        let req = Request::builder().uri("/200").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // Drop the body without polling it (simulates a client that disconnects
        // before reading the response).
        drop(resp);
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.status_class, 2);
        assert_eq!(ev.bytes, 0, "no bytes streamed before drop");
    }

    #[tokio::test]
    async fn skips_dashboard_path() {
        let clock = Arc::new(MockClock::new(0));
        let (tx, mut rx) = mpsc::channel::<StatEvent>(16);
        let handle = RecorderHandle::new(tx, clock, Arc::from("/__stats__"));
        let app = echo_router().layer(StatsRecorderLayer::new(handle));

        let req = Request::builder()
            .uri("/__stats__/something")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let _ = drain(resp).await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn channel_full_increments_dropped() {
        let clock = Arc::new(MockClock::new(0));
        let (tx, _rx) = mpsc::channel::<StatEvent>(1);
        tx.try_send(StatEvent {
            minute_ts: 0,
            path: "x".into(),
            status_class: 2,
            bytes: 0,
            country: None,
        })
        .unwrap();
        let handle = RecorderHandle::new(tx.clone(), clock, Arc::from("/__stats__"));
        let app = echo_router().layer(StatsRecorderLayer::new(handle.clone()));

        let req = Request::builder().uri("/200").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let _ = drain(resp).await;
        assert_eq!(handle.dropped(), 1);
    }

    struct FakeGeo;
    impl CountryResolver for FakeGeo {
        fn country_code(&self, ip: IpAddr) -> SmolStr {
            // Map a sentinel test IP to "US"; everything else "ZZ".
            if ip == "203.0.113.5".parse::<IpAddr>().unwrap() {
                SmolStr::new("US")
            } else {
                SmolStr::new("ZZ")
            }
        }
    }

    fn conn_info_req(uri: &str, ip: &str) -> Request<Body> {
        let mut req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let addr: SocketAddr = format!("{ip}:40000").parse().unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(addr));
        req
    }

    #[tokio::test]
    async fn records_country_from_peer_ip() {
        let clock = Arc::new(MockClock::new(0));
        let (tx, mut rx) = mpsc::channel::<StatEvent>(16);
        let handle = RecorderHandle::new(tx, clock, Arc::from("/__stats__"))
            .with_geo(Some(Arc::new(FakeGeo)), false);
        let app = echo_router().layer(StatsRecorderLayer::new(handle));

        let resp = app
            .oneshot(conn_info_req("/200", "203.0.113.5"))
            .await
            .unwrap();
        let _ = drain(resp).await;
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.country.as_deref(), Some("US"));
    }

    #[tokio::test]
    async fn xff_used_only_when_trusted() {
        // trust=false: header ignored, peer IP (sentinel) wins -> US.
        let clock = Arc::new(MockClock::new(0));
        let (tx, mut rx) = mpsc::channel::<StatEvent>(16);
        let handle = RecorderHandle::new(tx, clock, Arc::from("/__stats__"))
            .with_geo(Some(Arc::new(FakeGeo)), false);
        let app = echo_router().layer(StatsRecorderLayer::new(handle));
        let mut req = conn_info_req("/200", "203.0.113.5");
        req.headers_mut()
            .insert("x-forwarded-for", "8.8.8.8".parse().unwrap());
        let _ = drain(app.oneshot(req).await.unwrap()).await;
        assert_eq!(rx.recv().await.unwrap().country.as_deref(), Some("US"));

        // trust=true: the RIGHT-most XFF entry wins (the address our trusted
        // proxy appended), not the client-supplied left-most one. Sentinel is
        // right-most -> US, even though both the left-most entry and the socket
        // peer map to ZZ.
        let clock = Arc::new(MockClock::new(0));
        let (tx, mut rx) = mpsc::channel::<StatEvent>(16);
        let handle = RecorderHandle::new(tx, clock, Arc::from("/__stats__"))
            .with_geo(Some(Arc::new(FakeGeo)), true);
        let app = echo_router().layer(StatsRecorderLayer::new(handle));
        let mut req = conn_info_req("/200", "9.9.9.9");
        req.headers_mut()
            .insert("x-forwarded-for", "8.8.8.8, 203.0.113.5".parse().unwrap());
        let _ = drain(app.oneshot(req).await.unwrap()).await;
        assert_eq!(rx.recv().await.unwrap().country.as_deref(), Some("US"));
    }

    #[tokio::test]
    async fn xff_rightmost_spans_multiple_headers() {
        // The value may arrive as several header lines; the right-most entry of
        // the last line is the one the trusted proxy observed.
        let clock = Arc::new(MockClock::new(0));
        let (tx, mut rx) = mpsc::channel::<StatEvent>(16);
        let handle = RecorderHandle::new(tx, clock, Arc::from("/__stats__"))
            .with_geo(Some(Arc::new(FakeGeo)), true);
        let app = echo_router().layer(StatsRecorderLayer::new(handle));
        let mut req = conn_info_req("/200", "9.9.9.9");
        req.headers_mut()
            .append("x-forwarded-for", "1.1.1.1, 8.8.8.8".parse().unwrap());
        req.headers_mut()
            .append("x-forwarded-for", "203.0.113.5".parse().unwrap());
        let _ = drain(app.oneshot(req).await.unwrap()).await;
        assert_eq!(rx.recv().await.unwrap().country.as_deref(), Some("US"));
    }

    #[tokio::test]
    async fn no_country_when_geo_disabled() {
        let clock = Arc::new(MockClock::new(0));
        let (tx, mut rx) = mpsc::channel::<StatEvent>(16);
        let handle = RecorderHandle::new(tx, clock, Arc::from("/__stats__"));
        let app = echo_router().layer(StatsRecorderLayer::new(handle));
        let _ = drain(
            app.oneshot(conn_info_req("/200", "203.0.113.5"))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(rx.recv().await.unwrap().country, None);
    }

    #[tokio::test]
    async fn malformed_xff_falls_back_to_peer_when_trusted() {
        // trust=true but the forwarded header is not a valid IP: the resolver
        // must fall back to the socket peer (sentinel) rather than yield None.
        let clock = Arc::new(MockClock::new(0));
        let (tx, mut rx) = mpsc::channel::<StatEvent>(16);
        let handle = RecorderHandle::new(tx, clock, Arc::from("/__stats__"))
            .with_geo(Some(Arc::new(FakeGeo)), true);
        let app = echo_router().layer(StatsRecorderLayer::new(handle));
        let mut req = conn_info_req("/200", "203.0.113.5");
        req.headers_mut()
            .insert("x-forwarded-for", "not-an-ip".parse().unwrap());
        let _ = drain(app.oneshot(req).await.unwrap()).await;
        assert_eq!(rx.recv().await.unwrap().country.as_deref(), Some("US"));
    }

    #[tokio::test]
    async fn xff_rightmost_with_port_is_parsed_not_skipped() {
        // trust=true and the trusted proxy appends the observed peer WITH a port.
        // The right-most token must be parsed (port stripped) and win; it must
        // NOT be discarded in favor of the earlier, client-controlled entry.
        let clock = Arc::new(MockClock::new(0));
        let (tx, mut rx) = mpsc::channel::<StatEvent>(16);
        let handle = RecorderHandle::new(tx, clock, Arc::from("/__stats__"))
            .with_geo(Some(Arc::new(FakeGeo)), true);
        let app = echo_router().layer(StatsRecorderLayer::new(handle));
        let mut req = conn_info_req("/200", "9.9.9.9"); // socket peer -> ZZ
        // Left entry is client-supplied (-> ZZ); right-most is the sentinel the
        // proxy appended, carrying a port.
        req.headers_mut().insert(
            "x-forwarded-for",
            "8.8.8.8, 203.0.113.5:50000".parse().unwrap(),
        );
        let _ = drain(app.oneshot(req).await.unwrap()).await;
        assert_eq!(rx.recv().await.unwrap().country.as_deref(), Some("US"));
    }
}
