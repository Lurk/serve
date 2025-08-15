mod errors;
mod tls;

use axum::{http::StatusCode, Router};
use clap::{Args, Parser, Subcommand};
use clap_verbosity_flag::Verbosity;
use std::{
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
};
use tower_http::{
    compression::CompressionLayer,
    services::{ServeDir, ServeFile},
    set_status::SetStatus,
    trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer},
};
use tracing::Level;
use tracing_appender::{
    non_blocking::WorkerGuard,
    rolling::{RollingFileAppender, Rotation},
};

use crate::tls::start_tls_server;

#[derive(Args, Debug)]
struct Tls {
    /// path to the certificate file.
    #[clap(short, long)]
    cert: PathBuf,
    /// path to the private key file.
    #[clap(short, long)]
    key: PathBuf,
    /// Redirect HTTP to HTTPS. Works only if 443 port is used.
    #[clap(long)]
    redirect_http: bool,
}

#[derive(Subcommand, Debug)]
enum Subcommands {
    /// Adds TLS support
    Tls(Tls),
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct ServeArgs {
    #[clap(subcommand)]
    subcommand: Option<Subcommands>,
    /// Path to the directory to serve. Defaults to the current directory.
    path: Option<PathBuf>,
    /// Port to listen on.
    #[clap(short, long, default_value_t = 3000)]
    port: u16,
    /// Address to listen on.
    #[clap(short, long, default_value = "127.0.0.1")]
    addr: Ipv4Addr,
    /// Compression layer is enabled by default.
    #[clap(long)]
    disable_compression: bool,
    /// Path to 404 page. By default, 404 is empty.
    #[clap(long)]
    not_found: Option<PathBuf>,
    /// Override with 200 OK. Useful for SPA. Requires --not-found.
    #[clap(long, requires = "not_found")]
    ok: bool,
    /// Log level.
    #[command(flatten)]
    log_level: Verbosity,
    /// Path to the directory where logs will be stored. If not specified, logs will be printed to stdout.
    ///
    /// If specified, logs will be written to the file (log_path/serve.YYYY-MM-DD.log) and rotated daily.
    ///
    /// If the directory does not exist, it will be created.
    #[clap(long)]
    log_path: Option<PathBuf>,
    /// Maximum number of log files to keep. Defaults to 7.
    #[clap(long, requires = "log_path")]
    log_max_files: Option<usize>,
}

impl ServeArgs {
    pub fn get_path(&self) -> PathBuf {
        self.path.clone().unwrap_or(".".into())
    }
}

#[tokio::main]
async fn main() -> Result<(), errors::ServeError> {
    let args = ServeArgs::parse();
    let addr = SocketAddr::from((args.addr, args.port));

    let _guard: Option<WorkerGuard> =
        init_logging(&args.log_path, &args.log_max_files, &args.log_level)?;

    let serve_dir = ServeDir::new(args.get_path());

    let app = Router::new();

    let app = if let Some(path) = args.not_found.as_ref() {
        tracing::info!("custom 404 page");
        let serve_dir = if args.ok {
            tracing::info!("overriding 404 with 200 OK");
            serve_dir.fallback(SetStatus::new(ServeFile::new(path), StatusCode::OK))
        } else {
            serve_dir.not_found_service(ServeFile::new(path))
        };
        app.fallback_service(serve_dir)
    } else {
        app.fallback_service(serve_dir)
    };

    let app = if args.disable_compression {
        app
    } else {
        tracing::info!("compression enabled");
        app.layer(CompressionLayer::new())
    };

    let service = app
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .into_make_service();

    match args.subcommand {
        Some(Subcommands::Tls(tls)) => start_tls_server(service, addr, tls).await?,
        None => {
            tracing::info!("listening on {}", addr);
            axum_server::bind(addr).serve(service).await?;
        }
    };
    Ok(())
}

fn init_logging(
    log_path: &Option<PathBuf>,
    log_max_files: &Option<usize>,
    log_level: &Verbosity,
) -> Result<Option<WorkerGuard>, errors::ServeError> {
    if let Some(log_path) = log_path.as_ref() {
        if !log_path.exists() {
            std::fs::create_dir_all(log_path)?;
        } else if !log_path.is_dir() {
            return Err(errors::ServeError::NotADirectory(
                log_path.to_string_lossy().to_string(),
            ));
        }

        let file_appender = RollingFileAppender::builder()
            .rotation(Rotation::DAILY)
            .max_log_files(log_max_files.unwrap_or(7))
            .filename_prefix("serve")
            .filename_suffix("log")
            .build(log_path)?;
        let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

        tracing_subscriber::fmt()
            .with_max_level(*log_level)
            .with_writer(non_blocking)
            .compact()
            .init();

        return Ok(Some(_guard));
    }

    tracing_subscriber::fmt()
        .with_max_level(*log_level)
        .compact()
        .init();

    Ok(None)
}
