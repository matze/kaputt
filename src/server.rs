//! HTTP surface: overview, detail page, the HTMX status partial, the two RSS
//! feeds, `/about`, liveness and three embedded assets. The probe scheduler
//! runs alongside.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use anyhow::Context;
use askama::Template;
use askama_web::WebTemplate;
use axum::Router;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;

use crate::config::Config;
use crate::db::{self, Store, TargetState};
use crate::rollup;
use crate::scheduler;
use crate::views;

const STYLE: &str = include_str!("../assets/style.css");
const HTMX: &str = include_str!("../assets/htmx.min.js");
const GEIST: &[u8] = include_bytes!("../assets/fonts/geist-latin-wght.woff2");

/// How many incidents the detail page lists.
const DETAIL_INCIDENTS: u32 = 20;

/// Feeds change only on incident transitions, so a short public cache is
/// enough and saves rendering the same XML for every poll.
const FEED_CACHE: &str = "public, max-age=60";

#[derive(Clone)]
struct App {
    store: Store,
    base_url: String,
}

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

    // The rollup loop keeps the day bars, the uptime windows and retention up
    // to date; the first pass already ran before the listener was bound.
    let _rollup = rollup::spawn(store.clone());
    tracing::info!(listen = %config.listen, "serving");

    let pages = Router::new()
        .route("/", get(overview))
        .route("/partials/status", get(status_partial))
        .route("/t/{id}", get(detail))
        .route("/about", get(about))
        .layer(middleware::from_fn(no_store));

    let feeds = Router::new()
        .route("/feed.xml", get(feed_all))
        .route("/t/{id}/feed.xml", get(feed_target));

    let assets = Router::new()
        .route("/healthz", get(healthz))
        .route("/assets/style.css", get(style_css))
        .route("/assets/geist.woff2", get(geist_woff2))
        .route("/assets/htmx.min.js", get(htmx_js));

    let state = App {
        store,
        base_url: config.base_url,
    };

    let app = pages.merge(feeds).merge(assets).with_state(state);

    axum::serve(listener, app).await.context("server failed")?;

    Ok(())
}

/// Everything a request can fail with, mapped to a plain-text response.
enum AppError {
    NotFound,
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for AppError {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "Nicht gefunden\n").into_response(),
            Self::Internal(error) => {
                tracing::error!(%error, "request failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "Interner Fehler\n").into_response()
            }
        }
    }
}

/// Build the overview rows: target state plus the last 90 days of rollups per
/// target. The bars and the 90-day uptime come from the same rows.
async fn cards(
    store: &Store,
    states: &[TargetState],
    now: i64,
) -> anyhow::Result<Vec<views::TargetCard>> {
    let grid = views::day_grid(now);
    let mut cards = Vec::with_capacity(states.len());
    for state in states {
        let rows = store.daily_stats(&state.id, &grid.days[0]).await?;
        let stats = views::day_stats(&rows, &grid);
        cards.push(views::card(state, &stats, &grid, now));
    }
    Ok(cards)
}

async fn overview(State(app): State<App>) -> Result<impl IntoResponse, AppError> {
    let now = db::unix_now();
    let states = app.store.enabled_targets().await?;
    let targets = cards(&app.store, &states, now).await?;
    Ok(WebTemplate(views::OverviewPage { targets }))
}

/// The region HTMX replaces every 30 seconds; same markup as the page body.
async fn status_partial(State(app): State<App>) -> Result<impl IntoResponse, AppError> {
    let now = db::unix_now();
    let states = app.store.enabled_targets().await?;
    let targets = cards(&app.store, &states, now).await?;
    Ok(WebTemplate(views::StatusPartial { targets }))
}

/// All enabled targets, one entry per incident batch.
async fn feed_all(State(app): State<App>, headers: HeaderMap) -> Result<Response, AppError> {
    let now = db::unix_now();
    let since = views::feed_since(now) - views::FEED_BATCH_GAP_S;
    let incidents = app.store.feed_incidents(None, since, true).await?;
    let feed = views::feed_page(&app.base_url, None, &incidents, now);
    Ok(feed_response(feed, &headers)?)
}

/// One target's feed. Disabled targets keep it readable, like their page.
async fn feed_target(
    Path(id): Path<String>,
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let now = db::unix_now();
    let Some(state) = app.store.target(&id).await? else {
        return Err(AppError::NotFound);
    };
    let since = views::feed_since(now) - views::FEED_BATCH_GAP_S;
    let incidents = app.store.feed_incidents(Some(&id), since, false).await?;
    let feed = views::feed_page(&app.base_url, Some(&state), &incidents, now);
    Ok(feed_response(feed, &headers)?)
}

fn feed_response(feed: views::FeedPage, headers: &HeaderMap) -> anyhow::Result<Response> {
    let body = feed.render()?;
    let etag = etag_of(&body);
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        == Some(etag.as_str())
    {
        return Ok((
            StatusCode::NOT_MODIFIED,
            [
                (header::CACHE_CONTROL, FEED_CACHE.to_string()),
                (header::ETAG, etag),
            ],
        )
            .into_response());
    }
    Ok((
        [
            (
                header::CONTENT_TYPE,
                "application/rss+xml; charset=utf-8".to_string(),
            ),
            (header::CACHE_CONTROL, FEED_CACHE.to_string()),
            (header::ETAG, etag),
        ],
        body,
    )
        .into_response())
}

/// Weak validator over the rendered body. It changes only when the feed does,
/// so conditional requests save the render for unchanged incidents.
fn etag_of(body: &str) -> String {
    let mut hasher = DefaultHasher::new();
    body.hash(&mut hasher);
    format!("W/\"{:016x}\"", hasher.finish())
}

#[derive(Deserialize)]
struct DetailQuery {
    range: Option<String>,
}

async fn detail(
    Path(id): Path<String>,
    Query(query): Query<DetailQuery>,
    State(app): State<App>,
) -> Result<impl IntoResponse, AppError> {
    let now = db::unix_now();
    let Some(state) = app.store.target(&id).await? else {
        return Err(AppError::NotFound);
    };
    let range = query
        .range
        .as_deref()
        .and_then(views::ChartRange::parse)
        .unwrap_or_default();

    let grid = views::day_grid(now);
    let rows = app.store.daily_stats(&state.id, &grid.days[0]).await?;
    let stats = views::day_stats(&rows, &grid);
    let last_24h = app.store.history_since(&state.id, now - 24 * 3600).await?;
    let target = views::card(&state, &stats, &grid, now);
    let windows = views::windows(&stats, views::totals(&last_24h));

    let checks = app.store.chart_since(&id, now - range.seconds()).await?;
    let incidents = app
        .store
        .recent_incidents(&id, DETAIL_INCIDENTS)
        .await?
        .iter()
        .map(|incident| views::incident_view(incident, now))
        .collect();
    let interval_s = i64::from(state.interval_s);
    Ok(WebTemplate(views::DetailPage {
        target,
        url: state.url,
        windows,
        range_links: views::range_links(&state.id, range),
        chart: views::chart(&checks, interval_s, now, range),
        incidents,
        feed_href: views::feed_href(&state.id),
    }))
}

async fn about(State(app): State<App>) -> Result<impl IntoResponse, AppError> {
    let states = app.store.enabled_targets().await?;
    Ok(WebTemplate(views::AboutPage {
        targets: states
            .iter()
            .map(|state| views::TargetLink {
                name: state.name.clone(),
                url: state.url.clone(),
                interval: views::duration(i64::from(state.interval_s)),
            })
            .collect(),
    }))
}

/// HTML is never cached: a status page must not show an outage that ended.
/// Applied to the page routes (and their error responses) only, so assets keep
/// their own `Cache-Control`.
async fn no_store(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Liveness only: no database query, no outbound request to a target.
async fn healthz() -> &'static str {
    "ok"
}

async fn style_css() -> Response {
    // The stylesheet changes with a deploy, so it revalidates every time; the
    // vendored script never changes within a binary.
    asset("text/css; charset=utf-8", "no-cache", STYLE)
}

async fn htmx_js() -> Response {
    asset(
        "text/javascript; charset=utf-8",
        "public, max-age=86400",
        HTMX,
    )
}

async fn geist_woff2() -> Response {
    // One variable file, latin subset; it only changes with a deploy.
    asset("font/woff2", "public, max-age=86400", GEIST)
}

fn asset(content_type: &'static str, cache: &'static str, body: impl IntoResponse) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, cache),
        ],
        body,
    )
        .into_response()
}
