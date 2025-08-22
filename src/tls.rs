use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    time::Duration,
};

use axum::{
    body::Body,
    extract::Request,
    http::{header::HOST, Uri},
    response::{IntoResponse, Redirect, Response},
    routing::IntoMakeService,
    Router,
};
use axum_server::tls_rustls::RustlsConfig;
use clap::Args;
use notify::{
    event::{DataChange, ModifyKind},
    Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Result as NotifyResult, Watcher,
};
use serde::{Deserialize, Serialize};
use tokio::{join, runtime::Handle, time::sleep};
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::errors;

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
    service: IntoMakeService<Router>,
    addr: SocketAddr,
    tls: Tls,
) -> Result<(), errors::ServeError> {
    let config = RustlsConfig::from_pem_file(&tls.cert, &tls.key).await?;
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

    parts.authority = Some(
        https_host
            .replace("80", "443")
            .parse()
            .expect("host to be valid"),
    );

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
