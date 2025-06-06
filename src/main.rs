use axum::{http::StatusCode, Router};
use axum_server::tls_rustls::RustlsConfig;
use clap::{Args, Parser, Subcommand};
use clap_verbosity_flag::Verbosity;
use event::{DataChange, ModifyKind};
use notify::*;
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
    trace::{self, TraceLayer},
};
use tracing::Level;
use tracing_log::AsTrace;

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
    #[command(flatten)]
    log_level: Verbosity,
    /// compression layer is enabled by default.
    #[clap(long)]
    disable_compression: bool,
    /// path to 404 page. By default, 404 is empty.
    #[clap(long)]
    not_found: Option<PathBuf>,
    /// override with 200 OK. Useful for SPA. Requires --not-found.
    #[clap(long, requires = "not_found")]
    ok: bool,
}

impl ServeArgs {
    pub fn get_path(&self) -> PathBuf {
        self.path.clone().unwrap_or(".".into())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = ServeArgs::parse();
    let addr = SocketAddr::from((args.addr, args.port));

    tracing_subscriber::fmt()
        .with_max_level(args.log_level.log_level_filter().as_trace())
        .compact()
        .init();

    let serve_dir = ServeDir::new(args.get_path());

    let app = Router::new().layer(
        TraceLayer::new_for_http()
            .make_span_with(trace::DefaultMakeSpan::new().level(Level::INFO))
            .on_response(trace::DefaultOnResponse::new().level(Level::INFO)),
    );

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

    let service = app.into_make_service();

    match args.subcommand {
        Some(Subcommands::Tls(tls)) => {
            let config = RustlsConfig::from_pem_file(&tls.cert, &tls.key).await?;
            tracing::info!("listening on {} with TLS", addr);

            let (server, tls_watcher) = join!(
                axum_server::bind_rustls(addr, config.clone()).serve(service),
                init_certificate_watch(config, &tls)
            );
            server?;
            tls_watcher?;
        }
        None => {
            tracing::info!("listening on {}", addr);
            axum_server::bind(addr).serve(service).await?;
        }
    };
    Ok(())
}

async fn init_certificate_watch(
    tls_config: RustlsConfig,
    serve_config: &Tls,
) -> notify::Result<()> {
    let mut delay: u64 = 1;
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let rt = Handle::current();
    let retry_tx = tx.clone();

    let mut watcher = RecommendedWatcher::new(
        move |res: Result<Event>| match res {
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
                sleep(Duration::from_nanos(delay)).await;
                retry_tx
                    .send(())
                    .await
                    .expect("to be able to send retry message");
            }
        };
    }

    Ok(())
}
