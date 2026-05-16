use super::session::{CSRF_COOKIE, build_cookie, create_session_and_redirect, get_cookie};
use super::{StatsState, db};
use crate::stats::auth::{constant_time_eq, hash_password, random_token};
use crate::stats::store::Store;
use crate::stats::templates;
use axum::extract::{Form, State};
use axum::http::header::{HeaderMap, SET_COOKIE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

/// Render the initial password-setup page, or 404 if auth is already configured.
///
/// # Panics
/// Panics if the CSRF cookie value contains non-ASCII characters (impossible in practice).
pub async fn get_setup(State(st): State<StatsState>) -> Response {
    match db(st.store.clone(), Store::password_hash).await {
        Ok(Some(_)) => return StatusCode::NOT_FOUND.into_response(),
        Ok(None) => {}
        Err(e) => {
            tracing::error!(target: "serve::stats::routes", "password_hash lookup failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    }
    let csrf = random_token();
    let body = templates::render_setup(&csrf, None, &st.url_prefix);
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
pub struct SetupForm {
    pub csrf: String,
    pub setup_token: String,
    pub password: String,
    pub confirm: String,
}

pub async fn post_setup(
    State(st): State<StatsState>,
    headers: HeaderMap,
    Form(form): Form<SetupForm>,
) -> Response {
    match db(st.store.clone(), Store::password_hash).await {
        Ok(Some(_)) => return StatusCode::NOT_FOUND.into_response(),
        Ok(None) => {}
        Err(e) => {
            tracing::error!(target: "serve::stats::routes", "password_hash lookup failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    }
    let Some(csrf_cookie) = get_cookie(&headers, CSRF_COOKIE) else {
        return (StatusCode::BAD_REQUEST, "missing csrf cookie").into_response();
    };
    if !constant_time_eq(&csrf_cookie, &form.csrf) {
        return (StatusCode::BAD_REQUEST, "csrf mismatch").into_response();
    }
    let Some(expected_token) = st.setup_token.read() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !constant_time_eq(&expected_token, &form.setup_token) {
        st.metrics.record_failed_setup_token();
        return render_setup_err(&st, "invalid setup token");
    }
    if form.password.chars().count() < 12 {
        return render_setup_err(&st, "password must be at least 12 characters");
    }
    if form.password != form.confirm {
        return render_setup_err(&st, "passwords do not match");
    }
    let now = st.clock.now();
    let password = form.password.clone();
    let Ok(Ok(hash)) = tokio::task::spawn_blocking(move || hash_password(&password)).await else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "hash failed").into_response();
    };
    let set_hash = hash.clone();
    if let Err(e) = db(st.store.clone(), move |s| {
        s.set_password_hash(&set_hash, now)
    })
    .await
    {
        tracing::error!(target: "serve::stats::routes", "set_password_hash failed: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
    }
    st.setup_token.clear();
    create_session_and_redirect(&st, now).await
}

fn render_setup_err(st: &StatsState, msg: &str) -> Response {
    let csrf = random_token();
    let body = templates::render_setup(&csrf, Some(msg), &st.url_prefix);
    let mut resp = (StatusCode::BAD_REQUEST, Html(body)).into_response();
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

#[cfg(test)]
mod tests {
    use super::super::test_support::{setup_app, test_state};
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn get_setup_renders_form_when_no_auth() {
        let st = test_state(true);
        let app = setup_app(st);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/__stats__/setup")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let set_cookie = resp.headers().get(SET_COOKIE).unwrap().to_str().unwrap();
        assert!(set_cookie.contains("csrf_pre="));
    }

    #[tokio::test]
    async fn get_setup_404_when_auth_exists() {
        let st = test_state(false);
        st.store.set_password_hash("$argon2$blah", 0).unwrap();
        let app = setup_app(st);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/__stats__/setup")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn post_setup_csrf_missing_400() {
        let st = test_state(true);
        let app = setup_app(st);
        let body = "csrf=x&setup_token=y&password=longenoughpw1&confirm=longenoughpw1";
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/__stats__/setup")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn post_setup_happy_path_creates_hash_and_session() {
        let st = test_state(true);
        let setup_tok = st.setup_token.read().unwrap();
        let store = st.store.clone();
        let app = setup_app(st);

        let body = format!(
            "csrf=mycsrf&setup_token={setup_tok}&password=longenoughpw1&confirm=longenoughpw1"
        );
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/__stats__/setup")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("cookie", "csrf_pre=mycsrf")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert!(store.password_hash().unwrap().is_some());
        let set_cookies: Vec<_> = resp.headers().get_all(SET_COOKIE).iter().collect();
        assert!(
            set_cookies
                .iter()
                .any(|v| v.to_str().unwrap().starts_with("stats_session="))
        );
    }
}
