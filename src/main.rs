use axum::Router;
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf};
use tower_http::{services::ServeDir, trace::TraceLayer};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    /// path to the directory to serve. Defaults to the current directory.
    path: Option<PathBuf>,
    /// port to listen on. Defaults to 3000.
    #[clap(short, long)]
    port: Option<u16>,
}

impl Args {
    pub fn get_port(&self) -> u16 {
        self.port.unwrap_or(3000)
    }

    pub fn get_path(&self) -> PathBuf {
        self.path.clone().unwrap_or(".".into())
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();
    let args = Args::parse();
    let service = ServeDir::new(args.get_path());
    let app = Router::new()
        .nest_service("/", service)
        .layer(TraceLayer::new_for_http());

    let addr = SocketAddr::from(([127, 0, 0, 1], args.get_port()));
    tracing::debug!("listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}
