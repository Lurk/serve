use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::Response,
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use serve::{build_client, proxy_router, ProxyState};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;

/// Start a mock upstream server that echoes WebSocket messages
/// and responds with "ok" to regular HTTP GET requests.
async fn start_upstream() -> SocketAddr {
    async fn ws_handler(ws: WebSocketUpgrade) -> Response {
        ws.on_upgrade(handle_ws)
    }

    async fn handle_ws(mut socket: WebSocket) {
        while let Some(Ok(msg)) = socket.recv().await {
            match msg {
                Message::Text(text) => {
                    let echo = format!("echo: {}", text);
                    if socket.send(Message::Text(echo.into())).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    }

    async fn http_handler() -> &'static str {
        "ok"
    }

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/test_prefix/ws", get(ws_handler))
        .route("/health", get(http_handler));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    addr
}

/// Start the proxy server pointing at the given upstream.
fn build_proxy_app(upstream_addr: SocketAddr, prefix: &str, strip_prefix: bool) -> Router {
    let client = build_client();
    let state = ProxyState {
        client,
        upstream: format!("http://{}", upstream_addr),
        prefix: prefix.to_string(),
        strip_prefix,
    };

    Router::new().nest(prefix, proxy_router(state))
}

async fn start_proxy(upstream_addr: SocketAddr, prefix: &str, strip_prefix: bool) -> SocketAddr {
    let app = build_proxy_app(upstream_addr, prefix, strip_prefix);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    addr
}

#[tokio::test]
async fn test_websocket_proxy_basic() {
    let upstream_addr = start_upstream().await;
    let proxy_addr = start_proxy(upstream_addr, "/api", true).await;

    let url = format!("ws://{}/api/ws", proxy_addr);
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

    ws.send(TungsteniteMessage::Text("hello".into()))
        .await
        .unwrap();

    let response = ws.next().await.unwrap().unwrap();
    assert_eq!(response.into_text().unwrap(), "echo: hello");

    ws.send(TungsteniteMessage::Text("world".into()))
        .await
        .unwrap();

    let response = ws.next().await.unwrap().unwrap();
    assert_eq!(response.into_text().unwrap(), "echo: world");

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_websocket_proxy_with_strip_prefix_false() {
    let upstream_addr = start_upstream().await;
    let proxy_addr = start_proxy(upstream_addr, "/test_prefix", false).await;

    let url = format!("ws://{}/test_prefix/ws", proxy_addr);
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

    ws.send(TungsteniteMessage::Text("ping".into()))
        .await
        .unwrap();

    let response = ws.next().await.unwrap().unwrap();
    assert_eq!(response.into_text().unwrap(), "echo: ping");

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_regular_http_through_proxy() {
    let upstream_addr = start_upstream().await;
    let proxy_addr = start_proxy(upstream_addr, "/api", true).await;

    let client: Client<_, axum::body::Body> =
        Client::builder(TokioExecutor::new()).build_http();

    let uri: hyper::Uri = format!("http://{}/api/health", proxy_addr)
        .parse()
        .unwrap();
    let resp = client
        .get(uri)
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"ok");
}

#[tokio::test]
async fn test_websocket_upgrade_returns_101() {
    let upstream_addr = start_upstream().await;
    let proxy_addr = start_proxy(upstream_addr, "/api", true).await;

    let url = format!("ws://{}/api/ws", proxy_addr);
    let (_, response) = tokio_tungstenite::connect_async(&url).await.unwrap();

    assert_eq!(response.status(), 101);
}
