use axum::{
    extract::{
        ws::{CloseFrame as AxumCloseFrame, Message as AxumMessage, WebSocket, WebSocketUpgrade},
        ConnectInfo, FromRequestParts, Request, State,
    },
    http::{uri::Uri, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use tokio_tungstenite::tungstenite::{
    protocol::CloseFrame as TungsteniteCloseFrame, Message as TungsteniteMessage,
};

#[allow(dead_code)] // used as serde default
const fn default_strip_prefix() -> bool {
    true
}

#[allow(dead_code)] // constructed by serde
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

#[must_use]
pub fn build_client() -> Client<hyper_util::client::legacy::connect::HttpConnector, axum::body::Body>
{
    Client::builder(TokioExecutor::new()).build_http()
}

fn upstream_uri(state: &ProxyState, path_and_query: &str) -> Result<Uri, Response> {
    let upstream_path = if state.strip_prefix {
        path_and_query.to_string()
    } else {
        format!("{}{}", state.prefix.trim_end_matches('/'), path_and_query)
    };

    let uri_str = format!("{}{}", state.upstream.trim_end_matches('/'), upstream_path);

    uri_str
        .parse::<Uri>()
        .map_err(|_| (StatusCode::BAD_GATEWAY, "Invalid upstream URI").into_response())
}

async fn proxy_handler(
    State(state): State<ProxyState>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    mut req: Request,
) -> Response {
    tracing::debug!(
        method = %req.method(),
        version = ?req.version(),
        upgrade = ?req.headers().get(axum::http::header::UPGRADE),
        connection = ?req.headers().get(axum::http::header::CONNECTION),
        "proxy request"
    );

    let is_ws = req
        .headers()
        .get(axum::http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));

    let path_and_query = req
        .uri()
        .path_and_query()
        .map_or("/", axum::http::uri::PathAndQuery::as_str)
        .to_string();

    if is_ws {
        let uri = match upstream_uri(&state, &path_and_query) {
            Ok(uri) => uri,
            Err(resp) => return resp,
        };

        let (mut parts, _body) = req.into_parts();
        match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
            Ok(ws_upgrade) => {
                tracing::info!(upstream = %uri, "WebSocket upgrade accepted, starting relay");
                ws_upgrade.on_upgrade(move |client_socket| ws_relay(client_socket, uri))
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    version = ?parts.version,
                    method = %parts.method,
                    upgrade = ?parts.headers.get(axum::http::header::UPGRADE),
                    connection = ?parts.headers.get(axum::http::header::CONNECTION),
                    sec_ws_version = ?parts.headers.get(axum::http::header::SEC_WEBSOCKET_VERSION),
                    "WebSocket upgrade extraction failed"
                );
                e.into_response()
            }
        }
    } else {
        let uri = match upstream_uri(&state, &path_and_query) {
            Ok(uri) => uri,
            Err(resp) => return resp,
        };

        *req.uri_mut() = uri;

        let headers = req.headers_mut();
        headers.remove(axum::http::header::HOST);
        headers.remove(axum::http::header::CONNECTION);

        if let Ok(val) = HeaderValue::from_str(&client_addr.ip().to_string()) {
            headers.insert("x-forwarded-for", val);
        }
        headers.insert("x-forwarded-proto", HeaderValue::from_static("http"));

        *req.version_mut() = axum::http::Version::HTTP_11;

        match state.client.request(req).await {
            Ok(resp) => {
                let (parts, body) = resp.into_parts();
                let body = body.map_err(axum::Error::new).boxed();
                Response::from_parts(parts, axum::body::Body::new(body))
            }
            Err(e) => {
                tracing::error!("proxy error: {}", e);
                (StatusCode::BAD_GATEWAY, format!("Bad Gateway: {e}")).into_response()
            }
        }
    }
}

async fn ws_relay(client_ws: WebSocket, upstream_uri: Uri) {
    let ws_uri = upstream_uri
        .to_string()
        .replacen("http://", "ws://", 1)
        .replacen("https://", "wss://", 1);

    let upstream_ws = match tokio_tungstenite::connect_async(&ws_uri).await {
        Ok((stream, _)) => stream,
        Err(e) => {
            tracing::error!("Failed to connect to upstream WebSocket {}: {}", ws_uri, e);
            return;
        }
    };

    let (mut client_sink, mut client_stream) = client_ws.split();
    let (mut upstream_sink, mut upstream_stream) = upstream_ws.split();

    let client_to_upstream = async {
        while let Some(Ok(msg)) = client_stream.next().await {
            if upstream_sink
                .send(axum_msg_to_tungstenite(msg))
                .await
                .is_err()
            {
                break;
            }
        }
    };

    let upstream_to_client = async {
        while let Some(Ok(msg)) = upstream_stream.next().await {
            if client_sink
                .send(tungstenite_msg_to_axum(msg))
                .await
                .is_err()
            {
                break;
            }
        }
    };

    tokio::select! {
        () = client_to_upstream => {}
        () = upstream_to_client => {}
    }
}

fn axum_msg_to_tungstenite(msg: AxumMessage) -> TungsteniteMessage {
    match msg {
        AxumMessage::Text(text) => TungsteniteMessage::Text(text.as_str().to_owned().into()),
        AxumMessage::Binary(data) => TungsteniteMessage::Binary(data),
        AxumMessage::Ping(data) => TungsteniteMessage::Ping(data),
        AxumMessage::Pong(data) => TungsteniteMessage::Pong(data),
        AxumMessage::Close(frame) => {
            TungsteniteMessage::Close(frame.map(|f| TungsteniteCloseFrame {
                code: f.code.into(),
                reason: f.reason.as_str().to_owned().into(),
            }))
        }
    }
}

fn tungstenite_msg_to_axum(msg: TungsteniteMessage) -> AxumMessage {
    match msg {
        TungsteniteMessage::Text(text) => AxumMessage::Text(text.as_str().into()),
        TungsteniteMessage::Binary(data) => AxumMessage::Binary(data),
        TungsteniteMessage::Ping(data) => AxumMessage::Ping(data),
        TungsteniteMessage::Pong(data) => AxumMessage::Pong(data),
        TungsteniteMessage::Close(frame) => AxumMessage::Close(frame.map(|f| AxumCloseFrame {
            code: f.code.into(),
            reason: f.reason.as_str().into(),
        })),
        TungsteniteMessage::Frame(_) => unreachable!("raw frames not expected"),
    }
}

pub fn proxy_router(state: ProxyState) -> Router {
    Router::new()
        .route("/{*path}", any(proxy_handler))
        .route("/", any(proxy_handler))
        .with_state(state)
}
