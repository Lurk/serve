use axum::Router;
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf};
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
    let args = Args::parse();
    let service = ServeDir::new(&args.path);
    let app = Router::new()
        .nest_service("/", service)
        .layer(TraceLayer::new_for_http());

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    tracing::debug!("listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}
