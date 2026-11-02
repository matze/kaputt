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
            error_detail: Some(truncate(detail)),
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

fn truncate(text: String) -> String {
    const MAX_CHARS: usize = 400;

    if text.chars().count() <= MAX_CHARS {
        text
    } else {
        let mut short: String = text.chars().take(MAX_CHARS).collect();
        short.push('…');
        short
    }
}

#[cfg(test)]
mod tests;
