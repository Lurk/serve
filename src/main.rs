use axum::Router;
use axum_server::tls_rustls::RustlsConfig;
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::{
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
};
use tower_http::{
    compression::CompressionLayer,
    services::ServeDir,
    trace::{self, TraceLayer},
};
use tracing::Level;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Debug)]
enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[derive(Args, Debug)]
struct Tls {
    /// path to the certificate file.
    #[clap(short, long)]
    cert: PathBuf,
    /// path to the private key file.
    #[clap(short, long)]
    key: PathBuf,
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
    /// path to the directory to serve. Defaults to the current directory.
    path: Option<PathBuf>,
    /// port to listen on.
    #[clap(short, long, default_value_t = 3000)]
    port: u16,
    /// address to listen on.
    #[clap(short, long, default_value = "127.0.0.1")]
    addr: Ipv4Addr,
    /// log level.
    #[clap(value_enum, default_value_t = LogLevel::Error, long, short)]
    log_level: LogLevel,
    /// disable compression.
    #[clap(long, short)]
    disable_compression: bool,
}

impl ServeArgs {
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
    let args = ServeArgs::parse();

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

    let app = if args.disable_compression {
        app
    } else {
        tracing::info!("compression enabled");
        app.layer(CompressionLayer::new())
    };

    let addr = SocketAddr::from((args.addr, args.port));

    match args.subcommand {
        Some(Subcommands::Tls(tls)) => {
            let config = match RustlsConfig::from_pem_file(tls.cert, tls.key).await {
                Ok(config) => config,
                Err(err) => panic!("Error loading TLS config: {}", err),
            };

            tracing::info!("listening on {} with TLS", addr);

            axum_server::bind_rustls(addr, config)
                .serve(app.into_make_service())
                .await
                .unwrap();
        }
        None => {
            tracing::info!("listening on {}", addr);

            axum_server::bind(addr)
                .serve(app.into_make_service())
                .await
                .unwrap();
        }
    }
}
