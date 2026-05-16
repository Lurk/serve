use super::StatsState;
use crate::stats::auth::random_token;
use axum::extract::{FromRef, FromRequestParts};
use axum::http::header::{COOKIE, HeaderMap, SET_COOKIE};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};

pub const SESSION_COOKIE: &str = "stats_session";
pub const CSRF_COOKIE: &str = "csrf_pre";

#[must_use]
pub fn get_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let cookies = headers.get(COOKIE)?.to_str().ok()?;
    for part in cookies.split(';') {
        let p = part.trim();
        if let Some(rest) = p.strip_prefix(name) {
            if let Some(val) = rest.strip_prefix('=') {
                return Some(val.to_string());
            }
        }
    }
    None
}

#[must_use]
pub fn build_cookie(
    name: &str,
    value: &str,
    max_age_secs: i64,
    secure_attr: bool,
    path: &str,
) -> String {
    let secure = if secure_attr { "; Secure" } else { "" };
    if max_age_secs <= 0 {
        format!("{name}=; HttpOnly; SameSite=Strict; Path={path}; Max-Age=0{secure}")
    } else {
        format!(
            "{name}={value}; HttpOnly; SameSite=Strict; Path={path}; Max-Age={max_age_secs}{secure}"
        )
    }
}

pub struct Session;

impl<S> FromRequestParts<S> for Session
where
    S: Send + Sync,
    StatsState: axum::extract::FromRef<S>,
{
    type Rejection = Response;
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let st = StatsState::from_ref(state);
        let Some(token) = get_cookie(&parts.headers, SESSION_COOKIE) else {
            return Err(Redirect::to(&st.url("/login")).into_response());
        };
        let now = st.clock.now();
        match super::db(st.store.clone(), move |s| s.session_valid(&token, now)).await {
            Ok(true) => Ok(Self),
            Ok(false) => Err(Redirect::to(&st.url("/login")).into_response()),
            Err(e) => {
                tracing::error!(target: "serve::stats::routes", "session_valid db error: {e}");
                Err(Redirect::to(&st.url("/login")).into_response())
            }
        }
    }
}

pub(super) async fn create_session_and_redirect(st: &StatsState, now: i64) -> Response {
    let token = random_token();
    let ttl = i64::from(st.session_ttl_days) * 86_400;
    let create_token = token.clone();
    let create_res = super::db(st.store.clone(), move |s| {
        s.create_session(&create_token, now, now + ttl)
    })
    .await;
    if create_res.is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, "session create failed").into_response();
    }
    let mut resp = Redirect::to(&st.url("")).into_response();
    let cookies = [
        build_cookie(
            SESSION_COOKIE,
            &token,
            ttl,
            st.secure_cookies,
            &st.url_prefix,
        ),
        build_cookie(CSRF_COOKIE, "", 0, st.secure_cookies, &st.url_prefix),
    ];
    for c in &cookies {
        resp.headers_mut()
            .append(SET_COOKIE, HeaderValue::from_str(c).unwrap());
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_cookie_finds_value() {
        let mut h = HeaderMap::new();
        h.insert(COOKIE, "a=1; stats_session=abc; b=2".parse().unwrap());
        assert_eq!(get_cookie(&h, "stats_session").as_deref(), Some("abc"));
        assert_eq!(get_cookie(&h, "missing"), None);
    }

    #[test]
    fn build_cookie_includes_attributes() {
        let s = build_cookie("stats_session", "tok", 86_400, false, "/__stats__");
        assert!(s.contains("HttpOnly"));
        assert!(s.contains("SameSite=Strict"));
        assert!(s.contains("Path=/__stats__"));
        assert!(s.contains("Max-Age=86400"));
        assert!(!s.contains("Secure"));
    }

    #[test]
    fn build_cookie_secure_when_tls() {
        let s = build_cookie("stats_session", "tok", 86_400, true, "/__stats__");
        assert!(s.contains("Secure"));
    }

    #[test]
    fn build_cookie_clear_when_zero() {
        let s = build_cookie("stats_session", "tok", 0, true, "/__stats__");
        assert!(s.contains("Max-Age=0"));
    }

    #[test]
    fn build_cookie_uses_custom_path() {
        let s = build_cookie("stats_session", "tok", 86_400, false, "/admin/stats");
        assert!(s.contains("Path=/admin/stats"));
    }
}
