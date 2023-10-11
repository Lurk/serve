use axum::Router;
use clap::{Parser, ValueEnum};
use std::{net::SocketAddr, path::PathBuf};
use tower_http::{
    services::ServeDir,
    trace::{self, TraceLayer},
};
use tracing::Level;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    /// path to the directory to serve. Defaults to the current directory.
    path: Option<PathBuf>,
    /// port to listen on.
    #[clap(short, long, default_value_t = 3000)]
    port: u16,
    /// log level.
    #[clap(value_enum, default_value_t = LogLevel::Error, long, short)]
    log_level: LogLevel,
}

impl Args {
    pub fn get_path(&self) -> PathBuf {
        self.path.clone().unwrap_or(".".into())
    }
}

impl From<LogLevel> for Level {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Error => Level::ERROR,
            LogLevel::Warn => Level::WARN,
            LogLevel::Info => Level::INFO,
            LogLevel::Debug => Level::DEBUG,
            LogLevel::Trace => Level::TRACE,
        }
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_max_level(<LogLevel as Into<Level>>::into(args.log_level))
        .compact()
        .init();

    let service = ServeDir::new(args.get_path());
    let app = Router::new().nest_service("/", service).layer(
        TraceLayer::new_for_http()
            .make_span_with(trace::DefaultMakeSpan::new().level(Level::INFO))
            .on_response(trace::DefaultOnResponse::new().level(Level::INFO)),
    );

    let addr = SocketAddr::from(([127, 0, 0, 1], args.port));
    tracing::info!("listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}
