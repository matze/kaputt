//! HTTP surface and process wiring. Only the liveness endpoint exists so far;
//! the UI pages come later. The probe scheduler runs alongside the server.

use anyhow::Context;
use axum::Router;
use axum::routing::get;

use crate::config::Config;
use crate::db::Store;
use crate::scheduler;

pub async fn run(config: Config, store: Store) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("cannot listen on {}", config.listen))?;

    let tasks = scheduler::spawn(&config, &store)?;
    if tasks.is_empty() {
        tracing::warn!("no enabled targets; the status page would stay empty");
    } else {
        tracing::info!(targets = tasks.len(), "scheduler started");
    }
    tracing::info!(listen = %config.listen, "serving");

    let app = Router::new().route("/healthz", get(healthz));
    axum::serve(listener, app).await.context("server failed")?;
    Ok(())
}

/// Liveness only: no database query, no outbound request to a target.
async fn healthz() -> &'static str {
    "ok"
}
