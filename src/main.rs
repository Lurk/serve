mod config;
mod errors;
mod proxy;
mod service;
mod tls;

use axum::{http::StatusCode, Router};
use clap::Parser;
use clap_verbosity_flag::Verbosity;
use std::{net::SocketAddr, path::PathBuf};
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

use crate::{
    config::{ServeArgs, Subcommands},
    proxy::{build_client, proxy_router, ProxyState},
    tls::start_tls_server,
};

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), errors::ServeError> {
    let args = ServeArgs::parse();

    // Handle management subcommands before server setup
    match &args.subcommand {
        Some(Subcommands::Init(init_args)) => return service::init(init_args),
        Some(Subcommands::Install(install_args)) => return service::install(install_args),
        Some(Subcommands::Uninstall) => return service::uninstall(),
        Some(Subcommands::Validate(validate_args)) => return service::validate(validate_args),
        Some(Subcommands::Restart) => return service::restart(),
        Some(Subcommands::Reload) => return service::reload(),
        Some(Subcommands::Status) => return service::status(),
        _ => {}
    }

    let args = args.resolve_config()?;
    let addr = SocketAddr::from((args.addr, args.port));

    let _guard: Option<WorkerGuard> =
        init_logging(args.log_path.as_ref(), args.log_max_files, args.log_level)?;

    let has_proxy = !args.proxy.is_empty();

    for route in &args.proxy {
        if route.upstream.starts_with("https://") {
            return Err(errors::ServeError::Proxy(
                "HTTPS upstreams are not supported".to_string(),
            ));
        }
    }

    let serve_dir = ServeDir::new(args.get_path());

    let mut app = Router::new();

    if has_proxy {
        let client = build_client();
        for route in &args.proxy {
            tracing::info!(
                "proxying {} -> {} (strip_prefix: {})",
                route.path,
                route.upstream,
                route.strip_prefix
            );
            let state = ProxyState {
                client: client.clone(),
                upstream: route.upstream.clone(),
                prefix: route.path.clone(),
                strip_prefix: route.strip_prefix,
            };
            app = app.nest(&route.path, proxy_router(state));
        }
    }

    let compression = if args.disable_compression {
        None
    } else {
        tracing::info!("compression enabled");
        Some(CompressionLayer::new())
    };

    let app = if let Some(path) = args.not_found.as_ref() {
        tracing::info!("custom 404 page");
        let serve_dir = if args.ok {
            tracing::info!("overriding 404 with 200 OK");
            serve_dir.fallback(SetStatus::new(ServeFile::new(path), StatusCode::OK))
        } else {
            serve_dir.not_found_service(ServeFile::new(path))
        };
        match compression {
            Some(layer) => {
                app.fallback_service(Router::new().fallback_service(serve_dir).layer(layer))
            }
            None => app.fallback_service(serve_dir),
        }
    } else {
        match compression {
            Some(layer) => {
                app.fallback_service(Router::new().fallback_service(serve_dir).layer(layer))
            }
            None => app.fallback_service(serve_dir),
        }
    };

    let app = app.layer(
        TraceLayer::new_for_http()
            .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
            .on_response(
                DefaultOnResponse::new()
                    .level(Level::INFO)
                    .latency_unit(tower_http::LatencyUnit::Micros)
                    .include_headers(true),
            ),
    );

    let service = app.into_make_service_with_connect_info::<SocketAddr>();

    match args.subcommand {
        Some(Subcommands::Tls(tls)) => start_tls_server(service, addr, tls).await?,
        None => {
            tracing::info!("listening on {}", addr);
            axum_server::bind(addr).serve(service).await?;
        }
        _ => unreachable!("management subcommands handled above"),
    }
    Ok(())
}

fn init_logging(
    log_path: Option<&PathBuf>,
    log_max_files: Option<usize>,
    log_level: Verbosity,
) -> Result<Option<WorkerGuard>, errors::ServeError> {
    if let Some(log_path) = log_path {
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
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

        tracing_subscriber::fmt()
            .with_max_level(log_level)
            .with_writer(non_blocking)
            .compact()
            .init();

        return Ok(Some(guard));
    }

    tracing_subscriber::fmt()
        .with_max_level(log_level)
        .compact()
        .init();

    Ok(None)
}
