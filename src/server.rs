//! HTTP surface. Session 1 only needs the liveness endpoint; the UI pages come
//! later.

use anyhow::Context;
use axum::Router;
use axum::routing::get;

use crate::config::Config;
use crate::db::Store;

pub async fn run(config: Config, _store: Store) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("cannot listen on {}", config.listen))?;
    tracing::info!(listen = %config.listen, "serving");

    let app = Router::new().route("/healthz", get(healthz));
    axum::serve(listener, app).await.context("server failed")?;
    Ok(())
}

/// Liveness only: no database query, no outbound request to a target.
async fn healthz() -> &'static str {
    "ok"
}
