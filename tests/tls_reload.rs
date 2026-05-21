use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use axum_server::tls_rustls::RustlsConfig;
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::ServerConfig;
use serve::tls::{Tls, build_server_config_from_bytes, init_certificate_watch};
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

    let watcher_config = tls_config.clone();
    let handle = tokio::spawn(async move {
        let _ = init_certificate_watch(watcher_config, &tls).await;
    });

    // Give the watcher a moment to install before triggering changes.
    sleep(Duration::from_millis(200)).await;
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
