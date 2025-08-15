mod errors;

use axum::{
    extract::Request,
    http::{header::HOST, StatusCode, Uri},
    response::Redirect,
    Router,
};
use axum_server::tls_rustls::RustlsConfig;
use clap::{Args, Parser, Subcommand};
use clap_verbosity_flag::Verbosity;
use notify::{
    event::{DataChange, ModifyKind},
    Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Result as NotifyResult, Watcher,
};
use std::{
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    time::Duration,
};
use tokio::{join, runtime::Handle, time::sleep};
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
        Some(Subcommands::Tls(tls)) => {
            let config = RustlsConfig::from_pem_file(&tls.cert, &tls.key).await?;
            tracing::info!("listening on {} with TLS", addr);

            let (server, http_to_https_redirect, tls_watcher) = join!(
                axum_server::bind_rustls(addr, config.clone()).serve(service),
                init_http_to_https_redirect(tls.redirect_http, args.port, args.addr),
                init_certificate_watch(config, &tls)
            );
            server?;
            http_to_https_redirect?;
            tls_watcher?;
        }
        None => {
            tracing::info!("listening on {}", addr);
            axum_server::bind(addr).serve(service).await?;
        }
    };
    Ok(())
}

async fn init_http_to_https_redirect(
    should_redirect: bool,
    port: u16,
    addr: Ipv4Addr,
) -> Result<(), errors::ServeError> {
    if should_redirect && port == 443 {
        tracing::info!("initializing redirect from HTTP to HTTPS");
        let http_addr = SocketAddr::from((addr, 80));
        let service = Router::new()
            .fallback(redirect)
            .layer(
                TraceLayer::new_for_http()
                    .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                    .on_response(DefaultOnResponse::new().level(Level::INFO)),
            )
            .into_make_service();
        axum_server::bind(http_addr).serve(service).await?;
    }

    if should_redirect && port != 443 {
        tracing::error!("HTTP to HTTPS redirect is enabled but HTTPS port is not 443");
    }

    Ok(())
}

async fn redirect(req: Request) -> Redirect {
    let mut parts = req.uri().clone().into_parts();
    parts.scheme = Some(axum::http::uri::Scheme::HTTPS);

    if parts.path_and_query.is_none() {
        parts.path_and_query = Some("/".parse().unwrap());
    }

    let host = req
        .headers()
        .get(HOST)
        .expect("HOST to be present in request");

    let https_host = host
        .to_str()
        .expect("HOST to be a valid str")
        .replace("80", "443");

    parts.authority = Some(https_host.parse().expect("host to be valid"));

    let destination = Uri::from_parts(parts).expect("Uri can be recustructed with HTTPS schema");
    Redirect::permanent(destination.to_string().as_str())
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

async fn init_certificate_watch(
    tls_config: RustlsConfig,
    serve_config: &Tls,
) -> Result<(), errors::ServeError> {
    let mut delay: u64 = 1;
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let rt = Handle::current();
    let retry_tx = tx.clone();

    let mut watcher = RecommendedWatcher::new(
        move |res: NotifyResult<Event>| match res {
            Ok(res) => {
                if let EventKind::Modify(ModifyKind::Data(DataChange::Content)) = res.kind {
                    let tx = tx.clone();
                    rt.spawn(async move {
                        tx.send(()).await.expect("to be able to send message");
                    });
                }
            }
            Err(e) => tracing::error!("watcher error: {}", e),
        },
        Config::default(),
    )?;

    watcher.watch(&serve_config.cert, RecursiveMode::NonRecursive)?;
    watcher.watch(&serve_config.key, RecursiveMode::NonRecursive)?;

    while rx.recv().await.is_some() {
        tracing::info!("reloading rustls configuration");
        match tls_config
            .reload_from_pem_file(serve_config.cert.clone(), serve_config.key.clone())
            .await
        {
            Ok(_) => {
                tracing::info!("rustls configuration reload successiful");
                delay = 1;
            }
            Err(e) => {
                delay *= 2;
                tracing::error!("rustls reload error: {}", e);
                tracing::info!("sleep {} nanoseconds before retry", delay);
                sleep(Duration::from_millis(delay)).await;
                retry_tx
                    .send(())
                    .await
                    .expect("to be able to send retry message");
            }
        };
    }

    Ok(())
}
