use tower_http::services::ServeDir;

fn build_app_for_test(
    serve_dir: &std::path::Path,
    stats_router: axum::Router,
    recorder_layer: serve::stats::StatsRecorderLayer,
) -> axum::Router {
    let files = ServeDir::new(serve_dir);
    axum::Router::new()
        .merge(stats_router)
        .fallback_service(files)
        .layer(recorder_layer)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap()
}

// GET the URL and fully drain the body. Fully draining is what guarantees the
// server-side response stream completes, which fires the recorder's on_done
// and puts the StatEvent in the writer's channel before we tear the test down.
async fn fetch(c: &reqwest::Client, url: &str) {
    let _ = c.get(url).send().await.unwrap().bytes().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_recording_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let serve_dir = tmp.path().join("public");
    std::fs::create_dir_all(&serve_dir).unwrap();
    std::fs::write(serve_dir.join("index.html"), b"<html>hello</html>").unwrap();
    std::fs::write(serve_dir.join("app.js"), b"console.log('ok');").unwrap();
    let db_path = tmp.path().join("stats.db");

    let stats_cfg = serve::stats::StatsConfig {
        db_path: db_path.clone(),
        session_ttl_days: 30,
        secure_cookies: None,
        url_prefix: None,
    };
    let stats = serve::stats::StatsHandle::start(&stats_cfg, false)
        .await
        .unwrap();
    let app = build_app_for_test(&serve_dir, stats.router.clone(), stats.recorder_layer());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        })
        .await
        .unwrap();
    });

    let c = client();
    fetch(&c, &format!("http://{addr}/index.html")).await;
    fetch(&c, &format!("http://{addr}/app.js")).await;
    fetch(&c, &format!("http://{addr}/no-such")).await;

    let _ = shutdown_tx.send(());
    let _ = server.await;
    stats.shutdown().await;

    let store = serve::stats::store::Store::open(&db_path).unwrap();
    let (req2xx, _bytes) = store
        .summary_2xx(serve::stats::store::BucketTable::Minute, 0)
        .unwrap();
    assert!(req2xx >= 2, "expected >=2 2xx, got {req2xx}");
    let top = store
        .top_assets(
            serve::stats::store::BucketTable::Minute,
            0,
            serve::stats::store::TopMetric::Requests,
            30,
        )
        .unwrap();
    let paths: Vec<&str> = top.iter().map(|r| r.path.as_str()).collect();
    assert!(
        paths.contains(&"/index.html"),
        "missing /index.html in {paths:?}"
    );
    assert!(paths.contains(&"/app.js"), "missing /app.js in {paths:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_login_dashboard_excludes_stats_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let serve_dir = tmp.path().join("public");
    std::fs::create_dir_all(&serve_dir).unwrap();
    std::fs::write(serve_dir.join("index.html"), b"<html>hello</html>").unwrap();
    let db_path = tmp.path().join("stats.db");

    {
        let store = serve::stats::store::Store::open(&db_path).unwrap();
        let hash = serve::stats::auth::hash_password("s3cret-pw-1234").unwrap();
        store.set_password_hash(&hash, 0).unwrap();
    }

    let stats_cfg = serve::stats::StatsConfig {
        db_path: db_path.clone(),
        session_ttl_days: 30,
        secure_cookies: None,
        url_prefix: None,
    };
    let stats = serve::stats::StatsHandle::start(&stats_cfg, false)
        .await
        .unwrap();
    let app = build_app_for_test(&serve_dir, stats.router.clone(), stats.recorder_layer());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        })
        .await
        .unwrap();
    });
    let url = |p: &str| format!("http://{addr}{p}");

    let c = client();

    let r = c.get(url("/__stats__")).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::SEE_OTHER);
    let _ = r.bytes().await.unwrap();

    let login_resp = c.get(url("/__stats__/login")).send().await.unwrap();
    let csrf = login_resp
        .headers()
        .get_all("set-cookie")
        .iter()
        .find_map(|v| {
            let s = v.to_str().ok()?;
            let rest = s.strip_prefix("csrf_pre=")?;
            Some(rest.split(';').next()?.to_string())
        })
        .expect("csrf_pre cookie in Set-Cookie header");
    let _ = login_resp.bytes().await.unwrap();

    let r = c
        .post(url("/__stats__/login"))
        .form(&[("csrf", csrf.as_str()), ("password", "s3cret-pw-1234")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::SEE_OTHER);
    let _ = r.bytes().await.unwrap();

    let r = c.get(url("/__stats__")).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::OK);
    let _ = r.bytes().await.unwrap();

    fetch(&c, &url("/index.html")).await;
    fetch(&c, &url("/__stats__")).await;
    fetch(&c, &url("/__stats__/api/summary?window=1d")).await;

    let _ = shutdown_tx.send(());
    let _ = server.await;
    stats.shutdown().await;

    let store = serve::stats::store::Store::open(&db_path).unwrap();
    let top = store
        .top_assets(
            serve::stats::store::BucketTable::Minute,
            0,
            serve::stats::store::TopMetric::Requests,
            100,
        )
        .unwrap();
    let stats_paths: Vec<&str> = top
        .iter()
        .map(|r| r.path.as_str())
        .filter(|p| p.starts_with("/__stats__"))
        .collect();
    assert!(stats_paths.is_empty(), "found stats paths: {stats_paths:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_restart_preserves_recorded_data() {
    let tmp = tempfile::tempdir().unwrap();
    let serve_dir = tmp.path().join("public");
    std::fs::create_dir_all(&serve_dir).unwrap();
    std::fs::write(serve_dir.join("a.txt"), b"AAAA").unwrap();
    let db_path = tmp.path().join("stats.db");

    {
        let stats_cfg = serve::stats::StatsConfig {
            db_path: db_path.clone(),
            session_ttl_days: 30,
            secure_cookies: None,
            url_prefix: None,
        };
        let stats = serve::stats::StatsHandle::start(&stats_cfg, false)
            .await
            .unwrap();
        let app = build_app_for_test(&serve_dir, stats.router.clone(), stats.recorder_layer());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await
            .unwrap();
        });

        let c = client();
        for _ in 0..5 {
            fetch(&c, &format!("http://{addr}/a.txt")).await;
        }
        let _ = tx.send(());
        let _ = server.await;
        stats.shutdown().await;
    }

    let store = serve::stats::store::Store::open(&db_path).unwrap();
    let (req2xx, _) = store
        .summary_2xx(serve::stats::store::BucketTable::Minute, 0)
        .unwrap();
    assert!(req2xx >= 5, "expected >=5 requests, got {req2xx}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_custom_url_prefix_skips_and_serves() {
    let tmp = tempfile::tempdir().unwrap();
    let serve_dir = tmp.path().join("public");
    std::fs::create_dir_all(&serve_dir).unwrap();
    std::fs::write(serve_dir.join("index.html"), b"<html>hello</html>").unwrap();
    let db_path = tmp.path().join("stats.db");

    let stats_cfg = serve::stats::StatsConfig {
        db_path: db_path.clone(),
        session_ttl_days: 30,
        secure_cookies: None,
        url_prefix: Some("/admin/stats".into()),
    };
    let stats = serve::stats::StatsHandle::start(&stats_cfg, false)
        .await
        .unwrap();
    let app = build_app_for_test(&serve_dir, stats.router.clone(), stats.recorder_layer());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        })
        .await
        .unwrap();
    });

    let c = client();

    // The default prefix should NOT be mounted.
    let r = c
        .get(format!("http://{addr}/__stats__/login"))
        .send()
        .await
        .unwrap();
    // Falls through to ServeDir which returns 404 for a missing file.
    assert_eq!(r.status(), reqwest::StatusCode::NOT_FOUND);
    let _ = r.bytes().await.unwrap();

    // The custom prefix should redirect to setup.
    let r = c
        .get(format!("http://{addr}/admin/stats"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::SEE_OTHER);
    assert!(
        r.headers()["location"]
            .to_str()
            .unwrap()
            .contains("/admin/stats/setup")
    );
    let _ = r.bytes().await.unwrap();

    // Setup page renders with the custom prefix baked into the form action.
    let r = c
        .get(format!("http://{addr}/admin/stats/setup"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::OK);
    let body = r.text().await.unwrap();
    assert!(body.contains("action=\"/admin/stats/setup\""));

    // Hitting the file server still records (not under the custom prefix).
    fetch(&c, &format!("http://{addr}/index.html")).await;
    fetch(&c, &format!("http://{addr}/admin/stats/setup")).await;

    let _ = shutdown_tx.send(());
    let _ = server.await;
    stats.shutdown().await;

    let store = serve::stats::store::Store::open(&db_path).unwrap();
    let top = store
        .top_assets(
            serve::stats::store::BucketTable::Minute,
            0,
            serve::stats::store::TopMetric::Requests,
            100,
        )
        .unwrap();
    let recorded: Vec<&str> = top.iter().map(|r| r.path.as_str()).collect();
    assert!(
        recorded.contains(&"/index.html"),
        "missing /index.html in {recorded:?}"
    );
    let admin_recorded: Vec<&str> = recorded
        .iter()
        .copied()
        .filter(|p| p.starts_with("/admin/stats"))
        .collect();
    assert!(
        admin_recorded.is_empty(),
        "custom prefix should be skipped: {admin_recorded:?}"
    );
}
