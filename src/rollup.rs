//! Daily rollups and retention.
//!
//! The pages read one `daily_stats` row per target and local day; the raw
//! `checks` rows only back the rolling 24 h figures, the latency chart and the
//! incident bookkeeping. A pass walks every day that still has raw checks but
//! no current rollup, writes it, and then prunes what the tables no longer
//! need.
//!
//! A check accounts for the time until the next check, capped at the target's
//! interval: five-minute probes therefore make every failed check five minutes
//! of downtime, and a gap in the data (host asleep, scheduler stopped) never
//! turns into invented downtime.

use std::time::Duration;

use crate::day;
use crate::db::{self, DailyStat, Store};

/// Raw checks are kept this long. The rollups cover the same window and more.
pub const RAW_RETENTION_DAYS: i64 = 90;
/// Rollups are kept this long.
pub const STATS_RETENTION_YEARS: i64 = 2;
/// How often the loop refreshes today's row, backfills gaps and prunes.
const PASS_INTERVAL: Duration = Duration::from_secs(60);

/// What one pass did, for the log line and the tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Report {
    /// Days written or refreshed.
    pub days: usize,
    /// Raw checks deleted by retention.
    pub pruned_checks: usize,
    /// Rollup rows deleted by retention.
    pub pruned_days: usize,
}

/// Fold one local day of checks into the row the page reads.
///
/// `checks` are the stored checks of `date` in time order, `(ts, ok,
/// latency_ms)`; anything outside `date` is ignored. Latency is averaged over
/// the successful checks only: a timeout is not a response time.
pub fn day_stat(
    date: impl Into<jiff::civil::Date>,
    checks: &[(i64, bool, Option<i64>)],
    interval_s: i64,
) -> DailyStat {
    let (start, end) = day::bounds(date.into());
    let interval = interval_s.max(1);
    let mut stat = DailyStat {
        day: day::key(day::date_of(start)),
        checks: 0,
        ok_checks: 0,
        down_seconds: 0,
        avg_latency_ms: None,
        max_latency_ms: None,
    };
    let mut latency_sum = 0i64;
    let mut latency_count = 0i64;

    for (index, &(ts, ok, latency)) in checks.iter().enumerate() {
        if ts < start || ts >= end {
            continue;
        }
        stat.checks += 1;
        if ok {
            stat.ok_checks += 1;
            if let Some(ms) = latency {
                latency_sum += ms;
                latency_count += 1;
                stat.max_latency_ms = Some(stat.max_latency_ms.map_or(ms, |max| max.max(ms)));
            }
            continue;
        }
        let next = checks
            .get(index + 1)
            .map_or(ts + interval, |&(next, ..)| next.min(ts + interval));
        let covered = (next.max(ts) - ts).max(0);
        let until = (ts + covered).min(end);
        stat.down_seconds += (until - ts.max(start)).max(0);
    }

    if latency_count > 0 {
        stat.avg_latency_ms = Some((latency_sum + latency_count / 2) / latency_count);
    }
    stat
}

/// Write every rollup the raw checks still owe, then apply retention.
///
/// The rollup is derived only from `checks` and replaces whole rows, so a pass
/// is idempotent and a day that was rolled while it was still running is
/// corrected by the next one.
pub async fn pass(store: &Store, now: i64) -> anyhow::Result<Report> {
    let today = day::date_of(now);
    let mut report = Report::default();

    for (target_id, interval_s) in store.rollup_targets().await? {
        let Some(first_ts) = store.first_check_ts(&target_id).await? else {
            continue;
        };
        // Re-roll the newest written day: it may have been written while it
        // was still growing. Without any rollup, start at the oldest check.
        let first = day::date_of(first_ts);
        let mut date = store
            .last_rolled_day(&target_id)
            .await?
            .and_then(|key| day::parse_key(&key))
            .map_or(first, |last| last.max(first));

        while date <= today {
            let (start, end) = day::bounds(date);
            let checks = store.checks_between(&target_id, start, end).await?;
            if !checks.is_empty() {
                store
                    .upsert_daily_stat(&target_id, &day_stat(date, &checks, interval_s))
                    .await?;
                report.days += 1;
            }
            date = day::shift(date, 1);
        }
    }

    // Only after every day that still has raw checks was rolled up may the old
    // checks go: they are the input of the rollup they belong to.
    report.pruned_checks = store
        .prune_checks(now - RAW_RETENTION_DAYS * 86_400)
        .await?;
    let stats_before = day::key(day::shift_years(today, STATS_RETENTION_YEARS));
    report.pruned_days = store.prune_daily_stats(&stats_before).await?;
    Ok(report)
}

/// Run [`pass`] right away and then every [`PASS_INTERVAL`]. A single loop for
/// the whole process: probing and rolling up stay independent of each other.
/// The future never completes on its own; the caller cancels it with the rest
/// of the tree.
pub async fn run(store: &Store) {
    loop {
        match pass(store, db::unix_now()).await {
            Ok(report) => tracing::debug!(
                days = report.days,
                pruned_checks = report.pruned_checks,
                pruned_days = report.pruned_days,
                "rollup pass"
            ),
            Err(err) => tracing::warn!(%err, "rollup pass failed"),
        }
        tokio::time::sleep(PASS_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests;
