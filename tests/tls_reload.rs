use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use axum_server::tls_rustls::RustlsConfig;
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::{
    ClientConfig, ClientConnection, DigitallySignedStruct, Error as RustlsError, ServerConfig,
    ServerConnection, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime, pem::PemObject};
use serve::tls::{
    Tls, build_server_config_from_bytes, install_certificate_watch, run_certificate_watch,
};
use tempfile::TempDir;
use tokio::time::sleep;

struct CertPair {
    cert_pem: String,
    key_pem: String,
}

fn gen_cert(cn: &str) -> CertPair {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec![cn.to_string()]).expect("rcgen");
    CertPair {
        cert_pem: cert.pem(),
        key_pem: signing_key.serialize_pem(),
    }
}

fn write_pair(cert_path: &Path, key_path: &Path, pair: &CertPair) {
    std::fs::write(cert_path, &pair.cert_pem).expect("write cert");
    std::fs::write(key_path, &pair.key_pem).expect("write key");
}

async fn spawn_reloader(
    cert_path: PathBuf,
    key_path: PathBuf,
) -> (RustlsConfig, tokio::task::JoinHandle<()>) {
    let tls = Tls {
        cert: cert_path.clone(),
        key: key_path.clone(),
        redirect_http: false,
    };
    let initial_cert = std::fs::read(&cert_path).expect("read cert");
    let initial_key = std::fs::read(&key_path).expect("read key");
    let server_config =
        build_server_config_from_bytes(&initial_cert, &initial_key).expect("initial config");
    let tls_config = RustlsConfig::from_config(server_config);

    // install_certificate_watch returns only once the watcher's event stream is
    // live, so any change made after this point is guaranteed to be observed.
    // No fixed sleep, no flakiness.
    let state = install_certificate_watch(tls_config.clone(), &tls)
        .await
        .expect("install watcher");
    let handle = tokio::spawn(async move {
        run_certificate_watch(state).await;
    });
    (tls_config, handle)
}

async fn await_reload(
    tls_config: &RustlsConfig,
    initial: Arc<ServerConfig>,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        let current = tls_config.get_inner();
        if !Arc::ptr_eq(&initial, &current) {
            return true;
        }
        sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn inplace_rewrite_triggers_reload() {
    let dir = TempDir::new().unwrap();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");

    let v1 = gen_cert("v1.example");
    write_pair(&cert_path, &key_path, &v1);

    let (tls_config, _handle) = spawn_reloader(cert_path.clone(), key_path.clone()).await;
    let initial = tls_config.get_inner();

    let v2 = gen_cert("v2.example");
    write_pair(&cert_path, &key_path, &v2);

    assert!(
        await_reload(&tls_config, initial, Duration::from_secs(5)).await,
        "expected RustlsConfig to be reloaded after in-place rewrite"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn atomic_rename_triggers_reload() {
    let dir = TempDir::new().unwrap();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");

    let v1 = gen_cert("v1.example");
    write_pair(&cert_path, &key_path, &v1);

    let (tls_config, _handle) = spawn_reloader(cert_path.clone(), key_path.clone()).await;
    let initial = tls_config.get_inner();

    let v2 = gen_cert("v2.example");
    let cert_tmp = dir.path().join("cert.pem.tmp");
    let key_tmp = dir.path().join("key.pem.tmp");
    std::fs::write(&cert_tmp, &v2.cert_pem).unwrap();
    std::fs::write(&key_tmp, &v2.key_pem).unwrap();
    std::fs::rename(&cert_tmp, &cert_path).unwrap();
    std::fs::rename(&key_tmp, &key_path).unwrap();

    assert!(
        await_reload(&tls_config, initial, Duration::from_secs(5)).await,
        "expected RustlsConfig to be reloaded after atomic rename"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn certbot_symlink_swap_triggers_reload() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().unwrap();
    let live = root.path().join("live");
    let archive = root.path().join("archive");
    std::fs::create_dir(&live).unwrap();
    std::fs::create_dir(&archive).unwrap();

    let v1 = gen_cert("v1.example");
    let cert1 = archive.join("cert1.pem");
    let key1 = archive.join("key1.pem");
    std::fs::write(&cert1, &v1.cert_pem).unwrap();
    std::fs::write(&key1, &v1.key_pem).unwrap();
    let cert_link = live.join("cert.pem");
    let key_link = live.join("key.pem");
    symlink(&cert1, &cert_link).unwrap();
    symlink(&key1, &key_link).unwrap();

    let (tls_config, _handle) = spawn_reloader(cert_link.clone(), key_link.clone()).await;
    let initial = tls_config.get_inner();

    // Simulate certbot renewal: write new archive files, then atomically swap symlinks.
    let v2 = gen_cert("v2.example");
    let cert2 = archive.join("cert2.pem");
    let key2 = archive.join("key2.pem");
    std::fs::write(&cert2, &v2.cert_pem).unwrap();
    std::fs::write(&key2, &v2.key_pem).unwrap();

    let cert_tmp = live.join("cert.pem.tmp");
    let key_tmp = live.join("key.pem.tmp");
    symlink(&cert2, &cert_tmp).unwrap();
    symlink(&key2, &key_tmp).unwrap();
    std::fs::rename(&cert_tmp, &cert_link).unwrap();
    std::fs::rename(&key_tmp, &key_link).unwrap();

    assert!(
        await_reload(&tls_config, initial, Duration::from_secs(5)).await,
        "expected RustlsConfig to be reloaded after certbot-style symlink swap"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn k8s_secret_swap_triggers_reload() {
    use std::os::unix::fs::symlink;

    let mount = TempDir::new().unwrap();
    let mount_root = mount.path();
    let v1_dir = mount_root.join("..v1");
    std::fs::create_dir(&v1_dir).unwrap();

    let v1 = gen_cert("v1.example");
    std::fs::write(v1_dir.join("tls.crt"), &v1.cert_pem).unwrap();
    std::fs::write(v1_dir.join("tls.key"), &v1.key_pem).unwrap();

    let data_link = mount_root.join("..data");
    symlink(&v1_dir, &data_link).unwrap();

    let cert_link = mount_root.join("tls.crt");
    let key_link = mount_root.join("tls.key");
    symlink(data_link.join("tls.crt"), &cert_link).unwrap();
    symlink(data_link.join("tls.key"), &key_link).unwrap();

    let (tls_config, _handle) = spawn_reloader(cert_link.clone(), key_link.clone()).await;
    let initial = tls_config.get_inner();

    let v2_dir = mount_root.join("..v2");
    std::fs::create_dir(&v2_dir).unwrap();
    let v2 = gen_cert("v2.example");
    std::fs::write(v2_dir.join("tls.crt"), &v2.cert_pem).unwrap();
    std::fs::write(v2_dir.join("tls.key"), &v2.key_pem).unwrap();

    let data_tmp = mount_root.join("..data.tmp");
    symlink(&v2_dir, &data_tmp).unwrap();
    std::fs::rename(&data_tmp, &data_link).unwrap();

    assert!(
        await_reload(&tls_config, initial, Duration::from_secs(5)).await,
        "expected RustlsConfig to be reloaded after K8s-style ..data symlink swap"
    );
}

// A verifier that accepts any server certificate. Used only to drive an
// in-memory handshake far enough to read back which leaf the server presents;
// the served cert's bytes — not the handshake's success — are what we assert on.
#[derive(Debug)]
struct AcceptAnyServerCert;

impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        // rcgen's default key is ECDSA P-256, so advertising that scheme is
        // enough for the server to pick a signature it can produce.
        vec![
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ED25519,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PKCS1_SHA256,
        ]
    }
}

/// Run an in-memory TLS handshake against `config` and return the DER of the
/// leaf certificate the server presents.
fn served_leaf_der(config: Arc<ServerConfig>, sni: &str) -> Vec<u8> {
    let client_config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert))
        .with_no_client_auth();
    let name = ServerName::try_from(sni)
        .expect("valid server name")
        .to_owned();
    let mut client =
        ClientConnection::new(Arc::new(client_config), name).expect("client connection");
    let mut server = ServerConnection::new(config).expect("server connection");

    for _ in 0..16 {
        let mut c2s = Vec::new();
        while client.wants_write() {
            client.write_tls(&mut c2s).unwrap();
        }
        let mut cursor = c2s.as_slice();
        while !cursor.is_empty() {
            if server.read_tls(&mut cursor).unwrap() == 0 {
                break;
            }
            server.process_new_packets().unwrap();
        }

        let mut s2c = Vec::new();
        while server.wants_write() {
            server.write_tls(&mut s2c).unwrap();
        }
        let mut cursor = s2c.as_slice();
        while !cursor.is_empty() {
            if client.read_tls(&mut cursor).unwrap() == 0 {
                break;
            }
            client.process_new_packets().unwrap();
        }

        if !client.is_handshaking() && !server.is_handshaking() {
            break;
        }
    }

    let chain = client
        .peer_certificates()
        .expect("server presented a certificate chain");
    chain[0].as_ref().to_vec()
}

fn leaf_der(cert_pem: &str) -> Vec<u8> {
    CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .next()
        .expect("PEM contains a certificate")
        .expect("certificate parses")
        .as_ref()
        .to_vec()
}

// A rotation that lands *before* the watcher is installed emits no event the
// watcher can see. install_certificate_watch must still apply it via the
// authoritative read it performs after the watcher goes live.
#[tokio::test(flavor = "multi_thread")]
async fn rotation_during_install_is_applied() {
    let dir = TempDir::new().unwrap();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");

    let v1 = gen_cert("v1.example");
    write_pair(&cert_path, &key_path, &v1);
    let initial_cfg = build_server_config_from_bytes(v1.cert_pem.as_bytes(), v1.key_pem.as_bytes())
        .expect("initial config");
    let tls_config = RustlsConfig::from_config(initial_cfg);

    // Rotation happens before the watcher is installed.
    let v2 = gen_cert("v2.example");
    write_pair(&cert_path, &key_path, &v2);

    let tls = Tls {
        cert: cert_path.clone(),
        key: key_path.clone(),
        redirect_http: false,
    };
    let _state = install_certificate_watch(tls_config.clone(), &tls)
        .await
        .expect("install watcher");

    // Assert on cert *content*, not Arc identity: install always rebuilds the
    // config (its baseline hash never matches a real pair), so a pointer change
    // alone would pass even without the rotation. Handshaking against the live
    // config proves the served leaf is v2 — i.e. the authoritative post-install
    // read picked up the pre-install rotation, not the v1 it was first built from.
    let served = served_leaf_der(tls_config.get_inner(), "v2.example");
    assert_eq!(
        served,
        leaf_der(&v2.cert_pem),
        "expected the server to present the rotated v2 cert after install"
    );
    assert_ne!(
        served,
        leaf_der(&v1.cert_pem),
        "server must not still present the pre-rotation v1 cert"
    );
}
