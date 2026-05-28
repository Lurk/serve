use std::{
    io,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use axum::{
    Router,
    body::Body,
    extract::{Request, connect_info::IntoMakeServiceWithConnectInfo},
    http::{
        Uri,
        header::HOST,
        uri::{Authority, InvalidUri},
    },
    response::{IntoResponse, Redirect, Response},
};
use axum_server::tls_rustls::RustlsConfig;
use clap::Args;
use notify::{
    Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Result as NotifyResult, Watcher,
};
use rustls::ServerConfig;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use serde::{Deserialize, Serialize};
use tokio::{join, time::sleep};
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::errors;

/// Build a rustls `ServerConfig` from in-memory PEM-encoded cert and key bytes.
///
/// # Errors
///
/// Returns `ServeError::Io` (wrapping `io::ErrorKind::InvalidData`) if PEM
/// parsing fails or the key type is not supported by rustls.
pub fn build_server_config_from_bytes(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> Result<Arc<ServerConfig>, errors::ServeError> {
    let certs: Vec<CertificateDer> = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    let key = PrivateKeyDer::from_pem_slice(key_pem)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    Ok(Arc::new(config))
}

fn build_server_config(
    cert_path: &Path,
    key_path: &Path,
) -> Result<Arc<ServerConfig>, errors::ServeError> {
    let cert_pem = std::fs::read(cert_path)?;
    let key_pem = std::fs::read(key_path)?;
    build_server_config_from_bytes(&cert_pem, &key_pem)
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

/// Run the TLS-enabled HTTP server with hot-reloading of the certificate.
///
/// Binds `service` to `addr` using rustls, runs an optional HTTP-to-HTTPS
/// redirect on port 80, and watches the cert and key files for changes so
/// the server can reload them without restarting.
///
/// # Errors
///
/// Returns `ServeError::Io` if binding the socket fails, the cert or key
/// files cannot be read, or the watcher cannot install its inotify hooks.
/// Returns `ServeError::Notify` for filesystem-watcher errors.
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

fn bad_request() -> Response {
    Response::builder().status(400).body(Body::empty()).unwrap()
}

fn rewrite_authority_https(host: &str) -> Result<Authority, InvalidUri> {
    let authority = match host.rsplit_once(':') {
        Some((hostname, "80")) => format!("{hostname}:443"),
        _ => host.to_owned(),
    };
    authority.parse()
}

async fn redirect(req: Request) -> Response {
    let mut parts = req.uri().clone().into_parts();
    parts.scheme = Some(axum::http::uri::Scheme::HTTPS);

    if parts.path_and_query.is_none() {
        parts.path_and_query = Some("/".parse().expect("'/' to be valid 'path_and_query'"));
    }

    let Some(host) = req.headers().get(HOST) else {
        tracing::error!("HOST is not present in headers.");
        return bad_request();
    };

    let Ok(host_str) = host.to_str() else {
        tracing::error!("HOST from headers is not valid str.");
        return bad_request();
    };

    let Ok(authority) = rewrite_authority_https(host_str) else {
        tracing::error!("HOST from headers is not valid authority: {host_str}");
        return bad_request();
    };

    parts.authority = Some(authority);

    let Ok(destination) = Uri::from_parts(parts) else {
        tracing::error!("Url can not be reconstructed with HTTPS schema");
        return bad_request();
    };

    Redirect::permanent(destination.to_string().as_str()).into_response()
}

const RETRY_INITIAL: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);
const DEBOUNCE: Duration = Duration::from_secs(2);

fn hash_pair(cert: &[u8], key: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(cert);
    h.update(key);
    h.finalize().into()
}

/// Outcome of a single read + conditional reload of the cert/key pair.
enum ReloadOutcome {
    /// Pair changed and was successfully swapped in; carries the new hash.
    Reloaded([u8; 32]),
    /// Pair is byte-identical to `last_hash`; nothing to do.
    Unchanged,
    /// A cert or key read failed (transient); already logged.
    ReadFailed,
    /// The pair could not be built into a config (e.g. half-rotated); logged.
    BuildFailed,
}

/// Read the cert and key, and reload `tls_config` if the pair changed.
///
/// Reads are independent: observing a half-rotated pair yields `BuildFailed`,
/// which callers turn into a retry that picks up the consistent pair later.
async fn read_and_reload_if_changed(
    tls_config: &RustlsConfig,
    cert_path: &Path,
    key_path: &Path,
    last_hash: [u8; 32],
    context: &str,
) -> ReloadOutcome {
    let cert_bytes = match tokio::fs::read(cert_path).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("cert read failed during {context}: {e}");
            return ReloadOutcome::ReadFailed;
        }
    };
    let key_bytes = match tokio::fs::read(key_path).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("key read failed during {context}: {e}");
            return ReloadOutcome::ReadFailed;
        }
    };

    let new_hash = hash_pair(&cert_bytes, &key_bytes);
    if new_hash == last_hash {
        tracing::debug!("cert/key unchanged, skipping reload");
        return ReloadOutcome::Unchanged;
    }

    match build_server_config_from_bytes(&cert_bytes, &key_bytes) {
        Ok(new_config) => {
            tls_config.reload_from_config(new_config);
            log_cert_info(&cert_bytes, context);
            tracing::info!("{context}: rustls configuration applied");
            ReloadOutcome::Reloaded(new_hash)
        }
        Err(e) => {
            tracing::error!("rustls reload error: {e}");
            ReloadOutcome::BuildFailed
        }
    }
}

/// Live cert/key watcher plus the state needed to drive its reload loop.
///
/// Returned by [`install_certificate_watch`] once the watcher is live, and
/// consumed by [`run_certificate_watch`]. Holds the watcher so it stays
/// installed for the loop's lifetime.
#[must_use = "dropping WatchState stops the cert watcher; pass it to run_certificate_watch"]
pub struct WatchState {
    tls_config: RustlsConfig,
    cert: PathBuf,
    key: PathBuf,
    rx: tokio::sync::mpsc::Receiver<()>,
    retry_tx: tokio::sync::mpsc::Sender<()>,
    watcher: RecommendedWatcher,
    last_hash: [u8; 32],
}

/// Install the cert/key filesystem watcher and perform the authoritative
/// initial load.
///
/// Returns once the watcher's event stream is live (on macOS, after
/// `FSEventStreamStart`). Reading the cert/key strictly *after* the watcher is
/// live closes the startup window: a rotation before this read is reflected in
/// it, and a rotation after it emits an event. A transient read/parse failure
/// here is tolerated — the already-bound config is left in place and the load
/// is retried on the first watch event.
///
/// # Errors
///
/// Returns `ServeError::Notify` if the watcher cannot be created or cannot
/// install its hooks.
pub async fn install_certificate_watch(
    tls_config: RustlsConfig,
    serve_config: &Tls,
) -> Result<WatchState, errors::ServeError> {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let retry_tx = tx.clone();

    let mut watcher = RecommendedWatcher::new(
        move |res: NotifyResult<Event>| match res {
            Ok(event) => {
                if matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                    // 1-slot wake-up signal, not a queue: if a wake-up is already
                    // pending, drop this event — the loop reads current file state
                    // once it drains.
                    let _ = tx.try_send(());
                }
            }
            Err(e) => tracing::error!("watcher error: {}", e),
        },
        Config::default(),
    )?;

    let cert_dir = watch_dir(&serve_config.cert);
    let key_dir = watch_dir(&serve_config.key);
    watcher.watch(cert_dir, RecursiveMode::NonRecursive)?;
    if key_dir != cert_dir {
        watcher.watch(key_dir, RecursiveMode::NonRecursive)?;
    }

    // [0u8; 32] baseline forces this initial load; later changes arrive as events.
    let last_hash = if let ReloadOutcome::Reloaded(h) = read_and_reload_if_changed(
        &tls_config,
        &serve_config.cert,
        &serve_config.key,
        [0u8; 32],
        "rustls initial load",
    )
    .await
    {
        h
    } else {
        tracing::warn!("initial cert/key load deferred to first watch event");
        [0u8; 32]
    };

    Ok(WatchState {
        tls_config,
        cert: serve_config.cert.clone(),
        key: serve_config.key.clone(),
        rx,
        retry_tx,
        watcher,
        last_hash,
    })
}

/// Drive the cert/key reload loop until the watcher channel closes.
///
/// On each filesystem event the cert and key are re-read and, if changed,
/// atomically swapped into the shared `RustlsConfig`. A half-rotated pair fails
/// to build and schedules an exponential-backoff retry that picks up the
/// consistent pair on the next event. Returns once the watcher is dropped and
/// the event channel closes.
pub async fn run_certificate_watch(state: WatchState) {
    let WatchState {
        tls_config,
        cert,
        key,
        mut rx,
        retry_tx,
        // Bind (don't drop with bare `_`): the watcher must stay alive for the
        // loop's lifetime, otherwise filesystem events stop arriving.
        watcher: _watcher,
        mut last_hash,
    } = state;
    let mut delay = RETRY_INITIAL;

    while rx.recv().await.is_some() {
        sleep(DEBOUNCE).await;
        while rx.try_recv().is_ok() {}

        match read_and_reload_if_changed(&tls_config, &cert, &key, last_hash, "rustls reload").await
        {
            ReloadOutcome::Reloaded(h) => {
                last_hash = h;
                delay = RETRY_INITIAL;
            }
            ReloadOutcome::Unchanged | ReloadOutcome::ReadFailed => {}
            ReloadOutcome::BuildFailed => {
                delay = (delay * 2).min(RETRY_MAX);
                tracing::info!("sleep {:?} before retry", delay);
                sleep(delay).await;
                if retry_tx.send(()).await.is_err() {
                    tracing::warn!("certificate watcher channel closed, stopping retries");
                    break;
                }
            }
        }
    }
}

/// Install the cert/key watcher and run its reload loop forever.
///
/// Convenience wrapper over [`install_certificate_watch`] +
/// [`run_certificate_watch`] for the server startup path.
///
/// # Errors
///
/// Propagates watcher-installation errors from [`install_certificate_watch`].
pub async fn init_certificate_watch(
    tls_config: RustlsConfig,
    serve_config: &Tls,
) -> Result<(), errors::ServeError> {
    let state = install_certificate_watch(tls_config, serve_config).await?;
    run_certificate_watch(state).await;
    Ok(())
}

/// Directory to watch for changes to a cert or key file.
///
/// `Path::parent` returns `Some("")` for a bare filename and `None` only for
/// the filesystem root; in both cases we fall back to the current directory so
/// notify gets a real path to watch.
fn watch_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

fn log_cert_info(cert_pem: &[u8], context: &str) {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    use x509_parser::prelude::*;

    let Some(Ok(first_der)) = CertificateDer::pem_slice_iter(cert_pem).next() else {
        tracing::warn!("{context}: could not extract leaf cert from PEM for logging");
        return;
    };
    let der_bytes: &[u8] = first_der.as_ref();

    let parsed = match X509Certificate::from_der(der_bytes) {
        Ok((_, parsed)) => parsed,
        Err(e) => {
            tracing::warn!("{context}: x509 parse failed: {e}");
            return;
        }
    };

    let cn = parsed
        .subject()
        .iter_common_name()
        .next()
        .and_then(|attr| attr.as_str().ok())
        .unwrap_or("?");
    let sans = parsed
        .subject_alternative_name()
        .ok()
        .flatten()
        .map(|ext| {
            ext.value
                .general_names
                .iter()
                .filter_map(|gn| match gn {
                    GeneralName::DNSName(s) => Some((*s).to_string()),
                    GeneralName::IPAddress(bytes) => Some(format!("ip:{bytes:02x?}")),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    let not_after = parsed.validity().not_after.to_string();

    let fp = Sha256::digest(der_bytes);
    let mut fp_hex = String::with_capacity(16);
    for byte in &fp[..8] {
        let _ = write!(&mut fp_hex, "{byte:02x}");
    }

    tracing::info!(
        "{context}: subject_cn={cn} sans=[{sans}] not_after={not_after} fingerprint={fp_hex}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_authority_replaces_port_80() {
        let result = rewrite_authority_https("example.com:80").unwrap();
        assert_eq!(result.as_str(), "example.com:443");
    }

    #[test]
    fn rewrite_authority_preserves_host_containing_80() {
        let result = rewrite_authority_https("host80.example.com:80").unwrap();
        assert_eq!(result.as_str(), "host80.example.com:443");
    }

    #[test]
    fn rewrite_authority_preserves_ip_containing_80() {
        let result = rewrite_authority_https("180.0.0.1:80").unwrap();
        assert_eq!(result.as_str(), "180.0.0.1:443");
    }

    #[test]
    fn rewrite_authority_no_port() {
        let result = rewrite_authority_https("example.com").unwrap();
        assert_eq!(result.as_str(), "example.com");
    }

    #[test]
    fn rewrite_authority_non_80_port_unchanged() {
        let result = rewrite_authority_https("example.com:8080").unwrap();
        assert_eq!(result.as_str(), "example.com:8080");
    }

    #[test]
    fn watch_dir_for_bare_filename_is_current_dir() {
        assert_eq!(super::watch_dir(Path::new("cert.pem")), Path::new("."));
    }

    #[test]
    fn watch_dir_for_absolute_path_is_parent() {
        assert_eq!(
            super::watch_dir(Path::new("/etc/ssl/cert.pem")),
            Path::new("/etc/ssl")
        );
    }

    #[test]
    fn watch_dir_for_relative_with_dirs_is_parent() {
        assert_eq!(
            super::watch_dir(Path::new("certs/cert.pem")),
            Path::new("certs")
        );
    }

    #[test]
    fn build_server_config_from_bytes_rejects_partial_pem() {
        let truncated_cert = b"-----BEGIN CERTIFICATE-----\nMIIB";
        let truncated_key = b"-----BEGIN PRIVATE KEY-----\nMIIE";
        let result = super::build_server_config_from_bytes(truncated_cert, truncated_key);
        assert!(
            result.is_err(),
            "expected partial PEM to be rejected, got Ok"
        );
    }
}
