use super::session::{SESSION_COOKIE, get_cookie};
use super::{StatsState, db};
use crate::stats::store::Store;
use crate::stats::templates;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::HeaderMap;
use axum::response::{Html, IntoResponse, Redirect, Response};

pub async fn get_dashboard(State(st): State<StatsState>, headers: HeaderMap) -> Response {
    if st.setup_token.is_set() {
        match db(st.store.clone(), Store::password_hash).await {
            Ok(None) => return Redirect::to(&st.url("/setup")).into_response(),
            Ok(Some(_)) => {}
            Err(e) => {
                tracing::error!(target: "serve::stats::routes", "password_hash lookup failed: {e}");
                return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
            }
        }
    }
    let Some(token) = get_cookie(&headers, SESSION_COOKIE) else {
        return Redirect::to(&st.url("/login")).into_response();
    };
    let now = st.clock.now();
    let valid = match db(st.store.clone(), move |s| s.session_valid(&token, now)).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(target: "serve::stats::routes", "session_valid db error: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    };
    if !valid {
        return Redirect::to(&st.url("/login")).into_response();
    }
    Html(templates::render_dashboard(&st.url_prefix)).into_response()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{authed, body_string, full_app, test_state};
    use super::*;
    use crate::stats::auth::hash_password;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn dashboard_redirects_to_setup_when_no_auth() {
        let st = test_state(true);
        let app = full_app(st);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/__stats__")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert!(
            resp.headers()
                .get("location")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("/setup")
        );
    }

    #[tokio::test]
    async fn dashboard_redirects_to_login_when_no_session() {
        let st = test_state(false);
        st.store
            .set_password_hash(&hash_password("rightpassword1").unwrap(), 0)
            .unwrap();
        let app = full_app(st);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/__stats__")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert!(
            resp.headers()
                .get("location")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("/login")
        );
    }

    #[tokio::test]
    async fn dashboard_renders_when_session_valid() {
        let st = test_state(false);
        st.store
            .set_password_hash(&hash_password("rightpassword1").unwrap(), 0)
            .unwrap();
        st.store.create_session("tok-z", 0, 9_999_999_999).unwrap();
        let app = full_app(st);
        let resp = app.oneshot(authed("stats_session=tok-z")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;
        assert!(body.contains("serve · stats"));
    }
}
