use super::session::Session;
use super::{StatsState, db};
use crate::stats::latency::percentile_ms;
use crate::stats::store::{BucketTable, TopMetric};
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    Day1,
    Day7,
    Day30,
    Month12,
}

impl Window {
    #[must_use]
    pub fn from_query(s: &str) -> Option<Self> {
        match s {
            "1d" => Some(Self::Day1),
            "7d" => Some(Self::Day7),
            "30d" => Some(Self::Day30),
            "12m" => Some(Self::Month12),
            _ => None,
        }
    }
    #[must_use]
    pub const fn bucket_table(self) -> BucketTable {
        match self {
            Self::Day1 => BucketTable::Minute,
            Self::Day7 | Self::Day30 => BucketTable::Hour,
            Self::Month12 => BucketTable::Day,
        }
    }
    #[must_use]
    pub const fn since_seconds(self) -> i64 {
        match self {
            Self::Day1 => 86_400,
            Self::Day7 => 7 * 86_400,
            Self::Day30 => 30 * 86_400,
            Self::Month12 => 365 * 86_400,
        }
    }
    #[must_use]
    pub const fn granularity(self) -> &'static str {
        match self {
            Self::Day1 => "minute",
            Self::Day7 | Self::Day30 => "hour",
            Self::Month12 => "day",
        }
    }
}

#[derive(Deserialize)]
pub struct WindowQuery {
    pub window: String,
    #[serde(default)]
    pub sort: Option<String>,
}

#[derive(Serialize)]
pub struct TimeseriesResponse {
    pub window: String,
    pub granularity: &'static str,
    pub points: Vec<TimeseriesGroup>,
}

#[derive(Serialize)]
pub struct TimeseriesGroup {
    pub ts: i64,
    pub by_class: std::collections::HashMap<u8, ClassMetric>,
}

#[derive(Serialize)]
pub struct ClassMetric {
    pub requests: i64,
    pub bytes: i64,
}

#[derive(Serialize)]
pub struct CountryRow {
    pub country: String,
    pub requests: i64,
    pub bytes: i64,
    pub by_class: std::collections::HashMap<u8, ClassMetric>,
}

#[derive(Serialize)]
pub struct CountriesResponse {
    pub window: String,
    pub enabled: bool,
    pub rows: Vec<CountryRow>,
}

#[derive(Serialize)]
pub struct AssetsResponse {
    pub window: String,
    pub rows: Vec<crate::stats::store::AssetRow>,
}

#[derive(Serialize)]
pub struct ClassEntry {
    pub class: u8,
    pub requests: i64,
    pub bytes: i64,
}

#[derive(Serialize)]
pub struct SummaryResponse {
    pub window: String,
    pub requests_2xx: i64,
    pub bytes_2xx: i64,
    pub bytes_total: i64,
    pub by_class: Vec<ClassEntry>,
}

#[derive(Serialize)]
pub struct HealthResponse {
    pub dropped_events_since_startup: u64,
    pub sqlite_write_failures_since_startup: u64,
    pub last_flush_seconds_ago: Option<i64>,
    pub last_hour_rollup_seconds_ago: Option<i64>,
    pub last_day_rollup_seconds_ago: Option<i64>,
    pub failed_logins_since_startup: u64,
    pub failed_setup_token_attempts_since_startup: u64,
    pub last_failed_login_seconds_ago: Option<i64>,
}

#[derive(Serialize)]
pub struct SourceSummary {
    pub ttfb_p50: f64,
    pub ttfb_p95: f64,
    pub ttfb_p99: f64,
    pub total_p50: f64,
    pub total_p95: f64,
    pub total_p99: f64,
    pub not_modified_rate: f64,
    pub requests: i64,
}

#[derive(Serialize)]
pub struct SourceTsPoint {
    pub ts: i64,
    pub ttfb_p50: f64,
    pub ttfb_p95: f64,
    pub ttfb_p99: f64,
    pub total_p50: f64,
    pub total_p95: f64,
    pub total_p99: f64,
}

#[derive(Serialize)]
pub struct SourceBlock {
    pub source: String,
    pub summary: SourceSummary,
    pub timeseries: Vec<SourceTsPoint>,
}

#[derive(Serialize)]
pub struct LatencyResponse {
    pub window: String,
    pub granularity: &'static str,
    pub sources: Vec<SourceBlock>,
}

pub async fn get_timeseries(
    _s: Session,
    State(st): State<StatsState>,
    Query(q): Query<WindowQuery>,
) -> Response {
    let Some(win) = Window::from_query(&q.window) else {
        return (StatusCode::BAD_REQUEST, "invalid window").into_response();
    };
    let since = st.clock.now() - win.since_seconds();
    let table = win.bucket_table();
    let rows = match db(st.store.clone(), move |s| s.timeseries(table, since)).await {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    let mut groups: std::collections::BTreeMap<i64, std::collections::HashMap<u8, ClassMetric>> =
        std::collections::BTreeMap::default();
    for p in rows {
        groups.entry(p.ts).or_default().insert(
            p.status_class,
            ClassMetric {
                requests: p.requests,
                bytes: p.bytes,
            },
        );
    }
    let points = groups
        .into_iter()
        .map(|(ts, by_class)| TimeseriesGroup { ts, by_class })
        .collect();
    Json(TimeseriesResponse {
        window: q.window,
        granularity: win.granularity(),
        points,
    })
    .into_response()
}

pub async fn get_assets(
    _s: Session,
    State(st): State<StatsState>,
    Query(q): Query<WindowQuery>,
) -> Response {
    let Some(win) = Window::from_query(&q.window) else {
        return (StatusCode::BAD_REQUEST, "invalid window").into_response();
    };
    let metric = match q.sort.as_deref() {
        Some("requests") => TopMetric::Requests,
        _ => TopMetric::Bytes,
    };
    let since = st.clock.now() - win.since_seconds();
    let table = win.bucket_table();
    let rows = match db(st.store.clone(), move |s| {
        s.top_assets(table, since, metric, 30)
    })
    .await
    {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    Json(AssetsResponse {
        window: q.window,
        rows,
    })
    .into_response()
}

pub async fn get_countries(
    _s: Session,
    State(st): State<StatsState>,
    Query(q): Query<WindowQuery>,
) -> Response {
    let Some(win) = Window::from_query(&q.window) else {
        return (StatusCode::BAD_REQUEST, "invalid window").into_response();
    };
    if !st.geo_enabled {
        return Json(CountriesResponse {
            window: q.window,
            enabled: false,
            rows: Vec::new(),
        })
        .into_response();
    }
    // Default to bytes when `sort` is absent/unrecognized, matching get_assets.
    let by_requests = matches!(q.sort.as_deref(), Some("requests"));
    let since = st.clock.now() - win.since_seconds();
    let table = win.bucket_table();
    let breakdown = match db(st.store.clone(), move |s| s.country_breakdown(table, since)).await {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };

    // Group flat (country, class) rows into per-country totals + class map.
    let mut acc: std::collections::HashMap<String, CountryRow> = std::collections::HashMap::new();
    for r in breakdown {
        let row = acc.entry(r.country.clone()).or_insert_with(|| CountryRow {
            country: r.country.clone(),
            requests: 0,
            bytes: 0,
            by_class: std::collections::HashMap::new(),
        });
        row.requests = row.requests.saturating_add(r.requests);
        row.bytes = row.bytes.saturating_add(r.bytes);
        row.by_class.insert(
            r.status_class,
            ClassMetric {
                requests: r.requests,
                bytes: r.bytes,
            },
        );
    }
    let mut rows: Vec<CountryRow> = acc.into_values().collect();
    rows.sort_by(|a, b| {
        let key = if by_requests {
            b.requests.cmp(&a.requests)
        } else {
            b.bytes.cmp(&a.bytes)
        };
        // Stable tiebreak so equal counts order deterministically.
        key.then_with(|| a.country.cmp(&b.country))
    });
    rows.truncate(30);

    Json(CountriesResponse {
        window: q.window,
        enabled: true,
        rows,
    })
    .into_response()
}

pub async fn get_summary(
    _s: Session,
    State(st): State<StatsState>,
    Query(q): Query<WindowQuery>,
) -> Response {
    let Some(win) = Window::from_query(&q.window) else {
        return (StatusCode::BAD_REQUEST, "invalid window").into_response();
    };
    let since = st.clock.now() - win.since_seconds();
    let table = win.bucket_table();
    let (req2xx, bytes_2xx) = match db(st.store.clone(), move |s| s.summary_2xx(table, since)).await
    {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    let by_class = match db(st.store.clone(), move |s| {
        s.status_class_summary(table, since)
    })
    .await
    {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    let bytes_total = by_class.iter().map(|(_, _, b)| b).sum();
    Json(SummaryResponse {
        window: q.window,
        requests_2xx: req2xx,
        bytes_2xx,
        bytes_total,
        by_class: by_class
            .into_iter()
            .map(|(c, r, b)| ClassEntry {
                class: c,
                requests: r,
                bytes: b,
            })
            .collect(),
    })
    .into_response()
}

pub async fn get_health(_s: Session, State(st): State<StatsState>) -> Response {
    let now = st.clock.now();
    let last_hour: Option<i64> = db(st.store.clone(), |s| s.meta_get("last_hour_rollup_ts"))
        .await
        .unwrap_or(None)
        .and_then(|s| s.parse().ok());
    let last_day: Option<i64> = db(st.store.clone(), |s| s.meta_get("last_day_rollup_ts"))
        .await
        .unwrap_or(None)
        .and_then(|s| s.parse().ok());
    Json(HealthResponse {
        dropped_events_since_startup: st.recorder.dropped(),
        sqlite_write_failures_since_startup: st.writer.write_failures(),
        last_flush_seconds_ago: st.writer.last_flush_ts().map(|t| now - t),
        last_hour_rollup_seconds_ago: last_hour.map(|t| now - t),
        last_day_rollup_seconds_ago: last_day.map(|t| now - t),
        failed_logins_since_startup: st.metrics.failed_logins(),
        failed_setup_token_attempts_since_startup: st.metrics.failed_setup_tokens(),
        last_failed_login_seconds_ago: st.metrics.last_failed_login_ts().map(|t| now - t),
    })
    .into_response()
}

pub async fn get_latency(
    _s: Session,
    State(st): State<StatsState>,
    Query(q): Query<WindowQuery>,
) -> Response {
    let Some(win) = Window::from_query(&q.window) else {
        return (StatusCode::BAD_REQUEST, "invalid window").into_response();
    };
    let since = st.clock.now() - win.since_seconds();
    let table = win.bucket_table();

    let (totals, ts_rows) =
        match db(st.store.clone(), move |s| s.source_latency(table, since)).await {
            Ok(v) => v,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
        };

    let mut by_source: HashMap<&str, Vec<&_>> = HashMap::new();
    for r in &ts_rows {
        by_source.entry(r.source.as_str()).or_default().push(r);
    }

    let mut sources: Vec<SourceBlock> = totals
        .into_iter()
        .map(|t| {
            // 304s are already counted in `requests` (the writer bumps it for
            // every event), so the cache-hit fraction is not_modified / requests.
            #[allow(clippy::cast_precision_loss)]
            let not_modified_rate = if t.requests > 0 {
                t.not_modified as f64 / t.requests as f64
            } else {
                0.0
            };
            let timeseries = by_source
                .get(t.source.as_str())
                .into_iter()
                .flatten()
                .map(|r| SourceTsPoint {
                    ts: r.ts,
                    ttfb_p50: percentile_ms(&r.ttfb, 50.0),
                    ttfb_p95: percentile_ms(&r.ttfb, 95.0),
                    ttfb_p99: percentile_ms(&r.ttfb, 99.0),
                    total_p50: percentile_ms(&r.total, 50.0),
                    total_p95: percentile_ms(&r.total, 95.0),
                    total_p99: percentile_ms(&r.total, 99.0),
                })
                .collect();
            SourceBlock {
                summary: SourceSummary {
                    ttfb_p50: percentile_ms(&t.ttfb, 50.0),
                    ttfb_p95: percentile_ms(&t.ttfb, 95.0),
                    ttfb_p99: percentile_ms(&t.ttfb, 99.0),
                    total_p50: percentile_ms(&t.total, 50.0),
                    total_p95: percentile_ms(&t.total, 95.0),
                    total_p99: percentile_ms(&t.total, 99.0),
                    not_modified_rate,
                    requests: t.requests,
                },
                timeseries,
                source: t.source,
            }
        })
        .collect();
    sources.sort_by(|a, b| a.source.cmp(&b.source));

    Json(LatencyResponse {
        window: q.window,
        granularity: win.granularity(),
        sources,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{body_string, full_app, test_state};
    use super::*;
    use crate::stats::auth::hash_password;
    use crate::stats::store::Dimension;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[test]
    fn window_parse_and_attrs() {
        assert_eq!(Window::from_query("1d"), Some(Window::Day1));
        assert_eq!(Window::from_query("12m"), Some(Window::Month12));
        assert_eq!(Window::from_query("invalid"), None);
        assert_eq!(Window::Day1.bucket_table(), BucketTable::Minute);
        assert_eq!(Window::Day7.bucket_table(), BucketTable::Hour);
        assert_eq!(Window::Month12.bucket_table(), BucketTable::Day);
    }

    #[tokio::test]
    async fn api_endpoints_redirect_without_session() {
        let st = test_state(false);
        st.store
            .set_password_hash(&hash_password("rightpassword1").unwrap(), 0)
            .unwrap();
        let app = full_app(st);
        for path in [
            "/__stats__/api/summary?window=1d",
            "/__stats__/api/timeseries?window=1d",
            "/__stats__/api/assets?window=1d",
            "/__stats__/api/countries?window=1d",
            "/__stats__/api/latency?window=1d",
        ] {
            let resp = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::SEE_OTHER, "for {path}");
        }
    }

    #[tokio::test]
    async fn api_summary_invalid_window_400() {
        let st = test_state(false);
        st.store
            .set_password_hash(&hash_password("rightpassword1").unwrap(), 0)
            .unwrap();
        st.store.create_session("tok", 0, 9_999_999_999).unwrap();
        let app = full_app(st);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/__stats__/api/summary?window=bogus")
                    .header("cookie", "stats_session=tok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn api_summary_returns_aggregates() {
        let st = test_state(false);
        st.store
            .set_password_hash(&hash_password("rightpassword1").unwrap(), 0)
            .unwrap();
        st.store.create_session("tok", 0, 9_999_999_999).unwrap();
        st.store
            .upsert_minute(
                Dimension::Path,
                &[
                    crate::stats::store::MinuteRow::basic(
                        1_700_000_000 - 60,
                        "/a".into(),
                        2,
                        3,
                        300,
                    ),
                    crate::stats::store::MinuteRow::basic(
                        1_700_000_000 - 60,
                        "/b".into(),
                        4,
                        2,
                        42,
                    ),
                ],
            )
            .unwrap();
        let app = full_app(st);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/__stats__/api/summary?window=1d")
                    .header("cookie", "stats_session=tok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
        assert_eq!(body["requests_2xx"], 3);
        assert_eq!(body["bytes_2xx"], 300);
        assert_eq!(body["bytes_total"], 342);
        let by_class = body["by_class"].as_array().unwrap();
        let class4 = by_class
            .iter()
            .find(|e| e["class"] == 4)
            .expect("4xx entry");
        assert_eq!(class4["requests"], 2);
        assert_eq!(class4["bytes"], 42);
    }

    #[tokio::test]
    async fn api_countries_disabled_when_no_geo() {
        let st = test_state(false);
        st.store
            .set_password_hash(&hash_password("rightpassword1").unwrap(), 0)
            .unwrap();
        st.store.create_session("tok", 0, 9_999_999_999).unwrap();
        let app = full_app(st);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/__stats__/api/countries?window=1d")
                    .header("cookie", "stats_session=tok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
        assert_eq!(body["enabled"], false);
        assert!(body["rows"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn api_countries_ranks_and_breaks_down_by_class() {
        let mut st = test_state(false);
        st.geo_enabled = true;
        st.store
            .set_password_hash(&hash_password("rightpassword1").unwrap(), 0)
            .unwrap();
        st.store.create_session("tok", 0, 9_999_999_999).unwrap();
        st.store
            .upsert_minute(
                Dimension::Country,
                &[
                    crate::stats::store::MinuteRow::basic(
                        1_700_000_000 - 60,
                        "US".into(),
                        2,
                        10,
                        1000,
                    ),
                    crate::stats::store::MinuteRow::basic(
                        1_700_000_000 - 60,
                        "US".into(),
                        4,
                        2,
                        20,
                    ),
                    crate::stats::store::MinuteRow::basic(
                        1_700_000_000 - 60,
                        "DE".into(),
                        2,
                        3,
                        300,
                    ),
                ],
            )
            .unwrap();
        let app = full_app(st);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/__stats__/api/countries?window=1d&sort=requests")
                    .header("cookie", "stats_session=tok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
        assert_eq!(body["enabled"], true);
        let rows = body["rows"].as_array().unwrap();
        assert_eq!(rows[0]["country"], "US");
        assert_eq!(rows[0]["requests"], 12);
        assert_eq!(rows[0]["bytes"], 1020);
        assert_eq!(rows[0]["by_class"]["4"]["requests"], 2);
        assert_eq!(rows[1]["country"], "DE");
    }

    #[tokio::test]
    async fn api_latency_returns_per_source_percentiles() {
        use crate::stats::latency::N_BUCKETS;
        use crate::stats::store::SourceRow;
        let st = test_state(false);
        st.store
            .set_password_hash(&hash_password("rightpassword1").unwrap(), 0)
            .unwrap();
        st.store.create_session("tok", 0, 9_999_999_999).unwrap();
        // local: fast (bucket 1). proxy:/api: slow (top bucket) + 2 cache misses.
        let mut local_ttfb = [0u64; N_BUCKETS];
        local_ttfb[1] = 10;
        let mut slow_ttfb = [0u64; N_BUCKETS];
        slow_ttfb[12] = 5;
        st.store
            .upsert_source(&[
                SourceRow {
                    ts: 1_700_000_000 - 60,
                    source: "local".into(),
                    requests: 10,
                    not_modified: 5,
                    ttfb: local_ttfb,
                    total: local_ttfb,
                },
                SourceRow {
                    ts: 1_700_000_000 - 60,
                    source: "proxy:/api".into(),
                    requests: 5,
                    not_modified: 0,
                    ttfb: slow_ttfb,
                    total: slow_ttfb,
                },
            ])
            .unwrap();

        let app = full_app(st);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/__stats__/api/latency?window=1d")
                    .header("cookie", "stats_session=tok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
        let sources = body["sources"].as_array().unwrap();
        let proxy = sources
            .iter()
            .find(|s| s["source"] == "proxy:/api")
            .unwrap();
        let local = sources.iter().find(|s| s["source"] == "local").unwrap();
        // Slow source floors p99 at the top bound; fast source is far lower.
        assert!(proxy["summary"]["ttfb_p99"].as_f64().unwrap() >= 10_000.0);
        assert!(local["summary"]["ttfb_p95"].as_f64().unwrap() < 10.0);
        // not_modified_rate for local = 5 of 10 requests were 304 -> 0.5
        assert!((local["summary"]["not_modified_rate"].as_f64().unwrap() - 0.5).abs() < 1e-6);
        assert!(!proxy["timeseries"].as_array().unwrap().is_empty());
    }
}
