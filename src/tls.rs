use std::{
    io,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use axum::{
    body::Body,
    extract::{connect_info::IntoMakeServiceWithConnectInfo, Request},
    http::{
        header::HOST,
        uri::{Authority, InvalidUri},
        Uri,
    },
    response::{IntoResponse, Redirect, Response},
    Router,
};
use axum_server::tls_rustls::RustlsConfig;
use clap::Args;
use notify::{
    Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Result as NotifyResult, Watcher,
};
use rustls::ServerConfig;
use rustls_pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use serde::{Deserialize, Serialize};
use tokio::{join, runtime::Handle, time::sleep};
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::errors;

fn build_server_config(
    cert_path: &PathBuf,
    key_path: &PathBuf,
) -> Result<Arc<ServerConfig>, errors::ServeError> {
    let cert_pem = std::fs::read(cert_path)?;
    let key_pem = std::fs::read(key_path)?;

    let certs: Vec<CertificateDer> = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    let key = PrivateKeyDer::from_pem_slice(&key_pem)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    Ok(Arc::new(config))
}

#[derive(Args, Debug, Serialize, Deserialize, Clone)]
pub struct Tls {
    /// path to the certificate file.
    #[clap(short, long)]
    pub cert: PathBuf,
    /// path to the private key file.
    #[clap(short, long)]
    pub key: PathBuf,
    /// Redirect HTTP to HTTPS. Works only if 443 port is used.
    #[clap(long)]
    pub redirect_http: bool,
}

pub async fn start_tls_server(
    service: IntoMakeServiceWithConnectInfo<Router, SocketAddr>,
    addr: SocketAddr,
    tls: Tls,
) -> Result<(), errors::ServeError> {
    let config = RustlsConfig::from_config(build_server_config(&tls.cert, &tls.key)?);
    tracing::info!("listening on {} with TLS", addr);

    let (server, http_to_https_redirect, tls_watcher) = join!(
        axum_server::bind_rustls(addr, config.clone()).serve(service),
        init_http_to_https_redirect(tls.redirect_http, addr.port(), addr.ip()),
        init_certificate_watch(config, &tls)
    );
    server?;
    http_to_https_redirect?;
    tls_watcher?;
    Ok(())
}

async fn init_http_to_https_redirect(
    should_redirect: bool,
    port: u16,
    addr: IpAddr,
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

async fn redirect(req: Request) -> Response {
    let mut parts = req.uri().clone().into_parts();
    parts.scheme = Some(axum::http::uri::Scheme::HTTPS);

    if parts.path_and_query.is_none() {
        parts.path_and_query = Some("/".parse().expect("'/' to be valid 'path_and_query'"));
    }

    let Some(host) = req.headers().get(HOST) else {
        tracing::error!("HOST is not present in headers.");
        return Response::builder().status(400).body(Body::empty()).unwrap();
    };

    let Ok(https_host) = host.to_str() else {
        tracing::error!("HOST from headers is not valid str.");
        return Response::builder().status(400).body(Body::empty()).unwrap();
    };

    let authority: Result<Authority, InvalidUri> = https_host.replace("80", "443").parse();

    if authority.is_err() {
        tracing::error!("HOST from headers is not valid authority: {https_host}");
        return Response::builder().status(400).body(Body::empty()).unwrap();
    }

    parts.authority = Some(authority.expect("to be valid authority"));

    let Ok(destination) = Uri::from_parts(parts) else {
        tracing::error!("Url can not be reconstructed with HTTPS schema");
        return Response::builder().status(400).body(Body::empty()).unwrap();
    };

    Redirect::permanent(destination.to_string().as_str()).into_response()
}

async fn init_certificate_watch(
    tls_config: RustlsConfig,
    serve_config: &Tls,
) -> Result<(), errors::ServeError> {
    let mut delay: u64 = 1;
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let rt = Handle::current();
    let retry_tx = tx.clone();

    let cert_path =
        std::fs::canonicalize(&serve_config.cert).unwrap_or_else(|_| serve_config.cert.clone());
    let key_path =
        std::fs::canonicalize(&serve_config.key).unwrap_or_else(|_| serve_config.key.clone());

    let watched_cert = cert_path.clone();
    let watched_key = key_path.clone();

    let mut watcher = RecommendedWatcher::new(
        move |res: NotifyResult<Event>| match res {
            Ok(event) => {
                if matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                    let dominated = event.paths.iter().any(|p| {
                        let canonical = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
                        canonical == watched_cert || canonical == watched_key
                    });
                    if dominated {
                        let tx = tx.clone();
                        rt.spawn(async move {
                            let _ = tx.send(()).await;
                        });
                    }
                }
            }
            Err(e) => tracing::error!("watcher error: {}", e),
        },
        Config::default(),
    )?;

    let cert_dir = cert_path.parent().ok_or_else(|| {
        errors::ServeError::Notify(notify::Error::generic("cert path has no parent directory"))
    })?;
    let key_dir = key_path.parent().ok_or_else(|| {
        errors::ServeError::Notify(notify::Error::generic("key path has no parent directory"))
    })?;

    watcher.watch(cert_dir, RecursiveMode::NonRecursive)?;
    if key_dir != cert_dir {
        watcher.watch(key_dir, RecursiveMode::NonRecursive)?;
    }

    while rx.recv().await.is_some() {
        sleep(Duration::from_secs(2)).await;
        while rx.try_recv().is_ok() {}

        tracing::info!("reloading rustls configuration");
        match build_server_config(&serve_config.cert, &serve_config.key) {
            Ok(new_config) => {
                tls_config.reload_from_config(new_config);
                tracing::info!("rustls configuration reload successful");
                delay = 1;
            }
            Err(e) => {
                delay *= 2;
                tracing::error!("rustls reload error: {}", e);
                tracing::info!("sleep {} milliseconds before retry", delay);
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
