//! HTTP probes: one GET per target, status-only evaluation.
//!
//! Only the response head is used: the body is dropped as soon as the status
//! line and headers arrive, so a huge or endless body cannot stall a check.

use std::error::Error as _;
use std::time::{Duration, Instant};

use crate::config::TargetConfig;

pub const USER_AGENT: &str = "kaputt.matze.lol/0.1 (+https://kaputt.matze.lol/about)";

/// Redirects followed before the check counts as `too_many_redirects`.
const MAX_REDIRECTS: usize = 5;
/// Upper bound for establishing a TCP/TLS connection; the per-target timeout
/// still bounds the whole request.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a check failed, in the vocabulary stored in `checks.error_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Dns,
    Connect,
    Tls,
    Timeout,
    Status,
    TooManyRedirects,
    Other,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Dns => "dns",
            ErrorKind::Connect => "connect",
            ErrorKind::Tls => "tls",
            ErrorKind::Timeout => "timeout",
            ErrorKind::Status => "status",
            ErrorKind::TooManyRedirects => "too_many_redirects",
            ErrorKind::Other => "other",
        }
    }
}

/// Result of a single probe, shaped for one row in `checks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub ok: bool,
    pub status_code: Option<u16>,
    pub latency_ms: Option<i64>,
    pub error_kind: Option<ErrorKind>,
    pub error_detail: Option<String>,
}

impl Outcome {
    fn success(status: u16, latency: Duration) -> Self {
        Outcome {
            ok: true,
            status_code: Some(status),
            latency_ms: Some(latency.as_millis() as i64),
            error_kind: None,
            error_detail: None,
        }
    }

    fn failure(
        kind: ErrorKind,
        detail: String,
        status_code: Option<u16>,
        latency: Option<Duration>,
    ) -> Self {
        Outcome {
            ok: false,
            status_code,
            latency_ms: latency.map(|d| d.as_millis() as i64),
            error_kind: Some(kind),
            error_detail: Some(truncate(detail, 400)),
        }
    }
}

pub struct Prober {
    client: reqwest::Client,
}

impl Prober {
    pub fn new() -> anyhow::Result<Self> {
        install_crypto_provider()?;
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
            .connect_timeout(CONNECT_TIMEOUT)
            .build()?;
        Ok(Self { client })
    }

    pub async fn probe(&self, target: &TargetConfig) -> Outcome {
        let started = Instant::now();
        let response = self
            .client
            .get(&target.url)
            .timeout(Duration::from_secs(target.timeout_s.into()))
            .send()
            .await;
        let latency = started.elapsed();

        match response {
            Ok(response) => {
                let status = response.status().as_u16();
                drop(response);
                if status == target.expect_status {
                    Outcome::success(status, latency)
                } else {
                    Outcome::failure(
                        ErrorKind::Status,
                        format!("HTTP {status}, erwartet {}", target.expect_status),
                        Some(status),
                        Some(latency),
                    )
                }
            }
            Err(err) => {
                let (kind, detail) = classify(&err);
                Outcome::failure(kind, detail, None, Some(latency))
            }
        }
    }
}

/// reqwest is built with `rustls-no-provider` (ring instead of the heavy
/// aws-lc build), so the process has to install a provider before the first
/// client is built.
fn install_crypto_provider() -> anyhow::Result<()> {
    // `install_default` is backed by a process-wide `OnceLock`; a later call
    // only returns the provider that is already installed, which is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();
    if rustls::crypto::CryptoProvider::get_default().is_some() {
        Ok(())
    } else {
        anyhow::bail!("cannot install the rustls ring crypto provider")
    }
}

/// Map a transport error onto the stored error vocabulary plus a readable
/// detail string. The full source chain is kept because reqwest's top-level
/// message ("error sending request") says nothing about the cause.
fn classify(err: &reqwest::Error) -> (ErrorKind, String) {
    let chain = error_chain(err);
    let joined = chain.join(": ");
    let lower = joined.to_ascii_lowercase();

    let kind = if err.is_timeout() {
        ErrorKind::Timeout
    } else if err.is_redirect() {
        ErrorKind::TooManyRedirects
    } else {
        classify_transport(&lower, err.is_connect())
    };

    (kind, joined)
}

/// DNS and TLS failures surface as connect errors, but a TLS error outside the
/// connect phase still has to be recognised.
fn classify_transport(lower: &str, is_connect: bool) -> ErrorKind {
    const DNS_MARKERS: &[&str] = &[
        "dns error",
        "failed to lookup address",
        "name or service not known",
    ];

    const TLS_MARKERS: &[&str] = &[
        "tls",
        "ssl",
        "certificate",
        "handshake",
        "unknownissuer",
        "notvalidforname",
    ];

    if is_connect && has_marker(lower, DNS_MARKERS) {
        ErrorKind::Dns
    } else if has_marker(lower, TLS_MARKERS) {
        ErrorKind::Tls
    } else if is_connect {
        ErrorKind::Connect
    } else {
        ErrorKind::Other
    }
}

fn has_marker(haystack: &str, markers: &[&str]) -> bool {
    markers.iter().any(|m| haystack.contains(m))
}

fn error_chain(err: &reqwest::Error) -> Vec<String> {
    let mut messages = vec![err.to_string()];
    let mut source = err.source();
    while let Some(err) = source {
        messages.push(err.to_string());
        source = err.source();
    }
    messages
}

fn truncate(text: String, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text
    } else {
        let mut short: String = text.chars().take(max_chars).collect();
        short.push('…');
        short
    }
}

#[cfg(test)]
mod tests {
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
        let short = truncate("ä".repeat(500), 400);
        assert_eq!(short.chars().count(), 401);
        assert!(short.ends_with('…'));
        assert_eq!(truncate("ok".into(), 400), "ok");
    }
}
