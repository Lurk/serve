use super::session::{
    CSRF_COOKIE, SESSION_COOKIE, build_cookie, create_session_and_redirect, get_cookie,
};
use super::{StatsState, db};
use crate::stats::auth::{constant_time_eq, random_token, verify_password};
use crate::stats::store::Store;
use crate::stats::templates;
use axum::extract::{Form, State};
use axum::http::header::{HeaderMap, SET_COOKIE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;

/// Render the login page, or redirect to setup if no password has been set.
///
/// # Panics
/// Panics if the CSRF cookie value contains non-ASCII characters (impossible in practice).
pub async fn get_login(State(st): State<StatsState>) -> Response {
    if st.setup_token.is_set() {
        return Redirect::to(&st.url("/setup")).into_response();
    }
    let csrf = random_token();
    let body = templates::render_login(&csrf, None, &st.url_prefix);
    let mut resp = Html(body).into_response();
    resp.headers_mut().append(
        SET_COOKIE,
        HeaderValue::from_str(&build_cookie(
            CSRF_COOKIE,
            &csrf,
            600,
            st.secure_cookies,
            &st.url_prefix,
        ))
        .unwrap(),
    );
    resp
}

#[derive(Deserialize)]
pub struct LoginForm {
    pub csrf: String,
    pub password: String,
}

/// Handle login form submission and issue a session cookie on success.
///
/// # Panics
/// Panics if the CSRF cookie value contains non-ASCII characters (impossible in practice).
pub async fn post_login(
    State(st): State<StatsState>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let Some(csrf_cookie) = get_cookie(&headers, CSRF_COOKIE) else {
        return (StatusCode::BAD_REQUEST, "missing csrf cookie").into_response();
    };
    if !constant_time_eq(&csrf_cookie, &form.csrf) {
        return (StatusCode::BAD_REQUEST, "csrf mismatch").into_response();
    }
    let hash = match db(st.store.clone(), Store::password_hash).await {
        Ok(Some(h)) => h,
        Ok(None) => return Redirect::to(&st.url("/setup")).into_response(),
        Err(e) => {
            tracing::error!(target: "serve::stats::routes", "password_hash lookup failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    };
    let password = form.password.clone();
    let ok =
        tokio::task::spawn_blocking(move || verify_password(&password, &hash).unwrap_or(false))
            .await
            .unwrap_or(false);
    if !ok {
        let now = st.clock.now();
        st.metrics.record_failed_login(now);
        // Throttle brute-force attempts. argon2 already costs ~50ms per try;
        // adding a fixed 1s caps the rate at ~1/sec from a single attacker.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let csrf = random_token();
        let body = templates::render_login(&csrf, Some("invalid password"), &st.url_prefix);
        let mut resp = (StatusCode::UNAUTHORIZED, Html(body)).into_response();
        resp.headers_mut().append(
            SET_COOKIE,
            HeaderValue::from_str(&build_cookie(
                CSRF_COOKIE,
                &csrf,
                600,
                st.secure_cookies,
                &st.url_prefix,
            ))
            .unwrap(),
        );
        return resp;
    }
    create_session_and_redirect(&st, st.clock.now()).await
}

/// Delete the session and redirect to the login page.
///
/// # Panics
/// Panics if the session cookie value contains non-ASCII characters (impossible in practice).
pub async fn post_logout(State(st): State<StatsState>, headers: HeaderMap) -> Response {
    if let Some(tok) = get_cookie(&headers, SESSION_COOKIE) {
        let _ = db(st.store.clone(), move |s| s.delete_session(&tok)).await;
    }
    let mut resp = Redirect::to(&st.url("/login")).into_response();
    resp.headers_mut().append(
        SET_COOKIE,
        HeaderValue::from_str(&build_cookie(
            SESSION_COOKIE,
            "",
            0,
            st.secure_cookies,
            &st.url_prefix,
        ))
        .unwrap(),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{login_app, test_state};
    use super::*;
    use crate::stats::auth::hash_password;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn get_login_returns_form_with_csrf() {
        let st = test_state(false);
        st.store
            .set_password_hash(&hash_password("longenoughpw1").unwrap(), 0)
            .unwrap();
        let app = login_app(st);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/__stats__/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let sc = resp.headers().get(SET_COOKIE).unwrap().to_str().unwrap();
        assert!(sc.starts_with("csrf_pre="));
    }

    #[tokio::test]
    async fn get_login_redirects_to_setup_when_not_set_up() {
        let st = test_state(true);
        let app = login_app(st);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/__stats__/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn post_login_wrong_password_increments_counter() {
        let st = test_state(false);
        st.store
            .set_password_hash(&hash_password("rightpassword1").unwrap(), 0)
            .unwrap();
        let metrics = st.metrics.clone();
        let app = login_app(st);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/__stats__/login")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("cookie", "csrf_pre=c")
                    .body(Body::from("csrf=c&password=wrongpassword1"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(metrics.failed_logins(), 1);
    }

    #[tokio::test]
    async fn post_login_right_password_creates_session() {
        let st = test_state(false);
        st.store
            .set_password_hash(&hash_password("rightpassword1").unwrap(), 0)
            .unwrap();
        let store = st.store.clone();
        let app = login_app(st);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/__stats__/login")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("cookie", "csrf_pre=c")
                    .body(Body::from("csrf=c&password=rightpassword1"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let count: i64 = {
            let conn = store.conn_for_test();
            conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn post_login_csrf_mismatch_400() {
        let st = test_state(false);
        st.store
            .set_password_hash(&hash_password("rightpassword1").unwrap(), 0)
            .unwrap();
        let app = login_app(st);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/__stats__/login")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("cookie", "csrf_pre=expected")
                    .body(Body::from("csrf=different&password=rightpassword1"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn post_logout_clears_cookie_and_deletes_session() {
        let st = test_state(false);
        st.store.create_session("tok-x", 0, 9999).unwrap();
        let store = st.store.clone();
        let app = login_app(st);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/__stats__/logout")
                    .header("cookie", "stats_session=tok-x")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let sc = resp.headers().get(SET_COOKIE).unwrap().to_str().unwrap();
        assert!(sc.contains("Max-Age=0"));
        let count: i64 = {
            let conn = store.conn_for_test();
            conn.query_row(
                "SELECT COUNT(*) FROM sessions WHERE token='tok-x'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(count, 0);
    }
}
