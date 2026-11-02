use super::*;
use axum::Router;
use axum::response::Redirect;
use axum::routing::get;

fn target(url: String, expect_status: u16) -> TargetConfig {
    TargetConfig {
        id: "stub".into(),
        name: "Stub".into(),
        url,
        expect_status,
        interval_s: 300,
        timeout_s: 5,
        fail_threshold: 2,
        ok_threshold: 2,
        enabled: true,
        sort: 0,
    }
}

async fn spawn_stub() -> String {
    let app = Router::new()
        .route("/ok", get(|| async { "ok" }))
        .route(
            "/error",
            get(|| async { (axum::http::StatusCode::SERVICE_UNAVAILABLE, "kaputt") }),
        )
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(10)).await;
                "spät"
            }),
        )
        .route("/r1", get(|| async { Redirect::to("/r2") }))
        .route("/r2", get(|| async { Redirect::to("/ok") }))
        .route("/loop", get(|| async { Redirect::to("/loop") }));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stub");
    let addr = listener.local_addr().expect("stub addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("stub server");
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn ok_status_is_a_success() {
    let base = spawn_stub().await;
    let prober = Prober::new().expect("prober");
    let outcome = prober.probe(&target(format!("{base}/ok"), 200)).await;
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.status_code, Some(200));
    assert!(outcome.latency_ms.is_some());
    assert_eq!(outcome.error_kind, None);
}

#[tokio::test]
async fn expected_status_can_be_non_200() {
    let base = spawn_stub().await;
    let prober = Prober::new().expect("prober");
    let outcome = prober.probe(&target(format!("{base}/error"), 503)).await;
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.status_code, Some(503));
}

#[tokio::test]
async fn wrong_status_is_classified_as_status() {
    let base = spawn_stub().await;
    let prober = Prober::new().expect("prober");
    let outcome = prober.probe(&target(format!("{base}/error"), 200)).await;
    assert!(!outcome.ok);
    assert_eq!(outcome.status_code, Some(503));
    assert_eq!(outcome.error_kind, Some(ErrorKind::Status));
    assert!(outcome.latency_ms.is_some());
    let detail = outcome.error_detail.expect("detail");
    assert!(detail.contains("503"), "{detail}");
}

#[tokio::test]
async fn redirects_are_followed_to_the_final_status() {
    let base = spawn_stub().await;
    let prober = Prober::new().expect("prober");
    let outcome = prober.probe(&target(format!("{base}/r1"), 200)).await;
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.status_code, Some(200));
}

#[tokio::test]
async fn redirect_loops_are_cut_off() {
    let base = spawn_stub().await;
    let prober = Prober::new().expect("prober");
    let outcome = prober.probe(&target(format!("{base}/loop"), 200)).await;
    assert!(!outcome.ok);
    assert_eq!(outcome.error_kind, Some(ErrorKind::TooManyRedirects));
    assert_eq!(outcome.status_code, None);
}

#[tokio::test]
async fn timeouts_are_classified_as_timeout() {
    let base = spawn_stub().await;
    let prober = Prober::new().expect("prober");
    let mut slow = target(format!("{base}/slow"), 200);
    slow.timeout_s = 1;
    let started = Instant::now();
    let outcome = prober.probe(&slow).await;
    assert!(!outcome.ok);
    assert_eq!(outcome.error_kind, Some(ErrorKind::Timeout));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn refused_connections_are_classified_as_connect() {
    let prober = Prober::new().expect("prober");
    // Port 1 on loopback has no listener in any sane environment.
    let outcome = prober
        .probe(&target("http://127.0.0.1:1/".into(), 200))
        .await;
    assert!(!outcome.ok);
    assert_eq!(outcome.error_kind, Some(ErrorKind::Connect));
}

#[test]
fn dns_failures_are_recognised() {
    assert_eq!(
        classify_transport(
            "dns error: failed to lookup address information: Name or service not known",
            true
        ),
        ErrorKind::Dns
    );
    assert_eq!(
        classify_transport(
            "failed to lookup address information: temporary failure in name resolution",
            true
        ),
        ErrorKind::Dns
    );
}

#[test]
fn tls_failures_are_recognised() {
    assert_eq!(
        classify_transport("invalid peer certificate: UnknownIssuer", true),
        ErrorKind::Tls
    );
    assert_eq!(
        classify_transport("tls handshake failed: certificate expired", true),
        ErrorKind::Tls
    );
    assert_eq!(
        classify_transport("connection refused (os error 111)", true),
        ErrorKind::Connect
    );
    assert_eq!(
        classify_transport("tls error after connect", false),
        ErrorKind::Tls
    );
    assert_eq!(
        classify_transport("connection reset", false),
        ErrorKind::Other
    );
}

#[test]
fn error_kinds_have_stable_names() {
    for kind in [
        ErrorKind::Dns,
        ErrorKind::Connect,
        ErrorKind::Tls,
        ErrorKind::Timeout,
        ErrorKind::Status,
        ErrorKind::TooManyRedirects,
        ErrorKind::Other,
    ] {
        let name = kind.as_str();
        assert!(!name.is_empty());
        assert!(name.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
    }
}

#[test]
fn long_details_are_truncated_on_char_boundaries() {
    let short = truncate("ä".repeat(500));
    assert_eq!(short.chars().count(), 401);
    assert!(short.ends_with('…'));
    assert_eq!(truncate("ok".into()), "ok");
}
