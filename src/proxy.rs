use axum::{
    extract::{ConnectInfo, Request, State},
    http::{uri::Uri, HeaderValue},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use http_body_util::BodyExt;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

fn default_strip_prefix() -> bool {
    true
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProxyRoute {
    pub path: String,
    pub upstream: String,
    #[serde(default = "default_strip_prefix")]
    pub strip_prefix: bool,
}

#[derive(Clone)]
pub struct ProxyState {
    pub client: Client<hyper_util::client::legacy::connect::HttpConnector, axum::body::Body>,
    pub upstream: String,
    pub prefix: String,
    pub strip_prefix: bool,
}

pub fn build_client() -> Client<hyper_util::client::legacy::connect::HttpConnector, axum::body::Body>
{
    Client::builder(TokioExecutor::new()).build_http()
}

async fn proxy_handler(
    State(state): State<ProxyState>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    mut req: Request,
) -> Response {
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");

    let upstream_path = if state.strip_prefix {
        // axum's nest already stripped the prefix, so path_and_query is relative
        path_and_query.to_string()
    } else {
        format!("{}{}", state.prefix.trim_end_matches('/'), path_and_query)
    };

    let uri = format!("{}{}", state.upstream.trim_end_matches('/'), upstream_path);

    let Ok(uri) = uri.parse::<Uri>() else {
        return (axum::http::StatusCode::BAD_GATEWAY, "Invalid upstream URI").into_response();
    };

    *req.uri_mut() = uri;

    // Set proxy headers
    let headers = req.headers_mut();
    headers.remove(axum::http::header::HOST);
    headers.remove(axum::http::header::CONNECTION);

    if let Ok(val) = HeaderValue::from_str(&client_addr.ip().to_string()) {
        headers.insert("x-forwarded-for", val);
    }
    headers.insert("x-forwarded-proto", HeaderValue::from_static("http"));

    match state.client.request(req).await {
        Ok(resp) => {
            let (parts, body) = resp.into_parts();
            let body = body.map_err(axum::Error::new).boxed();
            Response::from_parts(parts, axum::body::Body::new(body))
        }
        Err(e) => {
            tracing::error!("proxy error: {}", e);
            (
                axum::http::StatusCode::BAD_GATEWAY,
                format!("Bad Gateway: {}", e),
            )
                .into_response()
        }
    }
}

pub fn proxy_router(state: ProxyState) -> Router {
    Router::new()
        .route("/{*path}", any(proxy_handler))
        .route("/", any(proxy_handler))
        .with_state(state)
}
