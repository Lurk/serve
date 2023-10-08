use axum::{
    body::{boxed, Body, BoxBody},
    http::{Request, Response, StatusCode, Uri},
    routing::get,
    Router,
};
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tower::ServiceExt;
use tower_http::{services::ServeDir, trace::TraceLayer};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// path to the project directory
    #[arg(short, long)]
    path: PathBuf,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();
    let args = Arc::new(Args::parse());
    let app = Router::new().nest_service(
        "/",
        get({
            let shared_args = args.clone();
            move |uri| handler(uri, shared_args)
        })
        .layer(TraceLayer::new_for_http()),
    );

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    tracing::debug!("listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

async fn handler(uri: Uri, args: Arc<Args>) -> Result<Response<BoxBody>, (StatusCode, String)> {
    let res = get_static_file(uri.clone(), args.clone()).await?;

    if res.status() == StatusCode::OK {
        Ok(res)
    } else {
        Err((
            res.status(),
            StatusCode::canonical_reason(&res.status())
                .unwrap_or("Unknown")
                .to_string(),
        ))
    }
}

async fn get_static_file(
    uri: Uri,
    args: Arc<Args>,
) -> Result<Response<BoxBody>, (StatusCode, String)> {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();

    // `ServeDir` implements `tower::Service` so we can call it with `tower::ServiceExt::oneshot`
    match ServeDir::new(&args.path).oneshot(req).await {
        Ok(res) => Ok(res.map(boxed)),
        Err(err) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Something went wrong: {}", err),
        )),
    }
}
