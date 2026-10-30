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

use tokio::task::JoinHandle;

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
pub fn day_stat(date: impl Into<jiff::civil::Date>, checks: &[(i64, bool, Option<i64>)], interval_s: i64) -> DailyStat {
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
    report.pruned_checks = store.prune_checks(now - RAW_RETENTION_DAYS * 86_400).await?;
    let stats_before = day::key(day::shift_years(today, STATS_RETENTION_YEARS));
    report.pruned_days = store.prune_daily_stats(&stats_before).await?;
    Ok(report)
}

/// Run [`pass`] right away and then every [`PASS_INTERVAL`]. A single loop for
/// the whole process: probing and rolling up stay independent of each other.
pub fn spawn(store: Store) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match pass(&store, db::unix_now()).await {
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TargetConfig;
    use crate::probe::{ErrorKind, Outcome};
    use crate::views;
    use jiff::civil::Date;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn target() -> TargetConfig {
        TargetConfig {
            id: "opac".into(),
            name: "OPAC".into(),
            url: "https://opac.example.org/".into(),
            expect_status: 200,
            interval_s: 300,
            timeout_s: 10,
            fail_threshold: 2,
            ok_threshold: 2,
            enabled: true,
            sort: 0,
        }
    }

    fn up() -> Outcome {
        Outcome {
            ok: true,
            status_code: Some(200),
            latency_ms: Some(100),
            error_kind: None,
            error_detail: None,
        }
    }

    fn down() -> Outcome {
        Outcome {
            ok: false,
            status_code: None,
            latency_ms: Some(10_000),
            error_kind: Some(ErrorKind::Timeout),
            error_detail: Some("operation timed out".into()),
        }
    }

    struct Fixture {
        _dir: TempDir,
        path: PathBuf,
        store: Store,
    }

    async fn fixture() -> Fixture {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("kaputt.db");
        let store = Store::open(&path).await.expect("store opens");
        store
            .upsert_targets(&[target()])
            .await
            .expect("upsert targets");
        Fixture {
            _dir: dir,
            path,
            store,
        }
    }

    /// Insert a check every `step` seconds across the whole local day, calling
    /// `result` for the index of the check.
    async fn fill_day(
        store: &Store,
        date: Date,
        step: i64,
        result: impl Fn(usize) -> Outcome,
    ) -> usize {
        let (start, end) = day::bounds(date);
        let mut index = 0;
        let mut ts = start;
        while ts < end {
            store
                .insert_check("opac", ts, &result(index))
                .await
                .expect("insert check");
            index += 1;
            ts += step;
        }
        index
    }

    fn raw_checks(path: &std::path::Path) -> i64 {
        let conn = rusqlite::Connection::open(path).expect("raw open");
        conn.query_row("SELECT count(*) FROM checks", [], |row| row.get(0))
            .expect("count")
    }

    #[tokio::test]
    async fn a_fixture_day_yields_exact_numbers() {
        let fixture = fixture().await;
        // 288 five-minute checks; every check of the fourth hour fails.
        let day = Date::new(2026, 6, 15).expect("date");
        let count = fill_day(&fixture.store, day, 300, |index| {
            if (48..52).contains(&index) {
                down()
            } else {
                up()
            }
        })
        .await;
        assert_eq!(count, 288);

        let report = pass(&fixture.store, day::bounds(day).1).await.expect("pass");
        assert_eq!(report.days, 1);

        let rows = fixture
            .store
            .daily_stats("opac", "2026-06-15")
            .await
            .expect("stats");
        assert_eq!(
            rows,
            vec![DailyStat {
                day: "2026-06-15".to_string(),
                checks: 288,
                ok_checks: 284,
                down_seconds: 4 * 300,
                avg_latency_ms: Some(100),
                max_latency_ms: Some(100),
            }]
        );
        // The page shows the rollup, not the checks.
        assert_eq!(views::percent(rows[0].ok_checks, rows[0].checks), "98,6 %");
    }

    #[tokio::test]
    async fn dst_days_are_twenty_five_and_twenty_three_hours_long() {
        let fixture = fixture().await;
        let long = Date::new(2026, 10, 25).expect("date");
        let short = Date::new(2026, 3, 29).expect("date");
        let long_checks = fill_day(&fixture.store, long, 300, |_| up()).await;
        let short_checks = fill_day(&fixture.store, short, 300, |_| down()).await;
        assert_eq!(long_checks, 300);
        assert_eq!(short_checks, 276);

        // Every check fails on the short day: the whole 23 hours are downtime.
        pass(&fixture.store, day::bounds(long).1).await.expect("pass");
        let rows = fixture
            .store
            .daily_stats("opac", "2026-03-29")
            .await
            .expect("stats");
        assert_eq!(
            rows,
            vec![
                DailyStat {
                    day: "2026-03-29".to_string(),
                    checks: 276,
                    ok_checks: 0,
                    down_seconds: 23 * 3600,
                    avg_latency_ms: None,
                    max_latency_ms: None,
                },
                DailyStat {
                    day: "2026-10-25".to_string(),
                    checks: 300,
                    ok_checks: 300,
                    down_seconds: 0,
                    avg_latency_ms: Some(100),
                    max_latency_ms: Some(100),
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_day_that_grows_is_rewritten_not_doubled() {
        let fixture = fixture().await;
        let day = Date::new(2026, 6, 15).expect("date");
        let start = day::bounds(day).0;
        for index in 0..10 {
            fixture
                .store
                .insert_check("opac", start + index * 300, &up())
                .await
                .expect("insert");
        }
        pass(&fixture.store, start + 3600).await.expect("first pass");
        for index in 10..20 {
            fixture
                .store
                .insert_check("opac", start + index * 300, &down())
                .await
                .expect("insert");
        }
        pass(&fixture.store, start + 7200).await.expect("second pass");

        let rows = fixture
            .store
            .daily_stats("opac", "2026-06-15")
            .await
            .expect("stats");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].checks, 20);
        assert_eq!(rows[0].ok_checks, 10);
        assert_eq!(rows[0].down_seconds, 10 * 300);
    }

    #[tokio::test]
    async fn retention_rolls_up_before_it_deletes() {
        let fixture = fixture().await;
        let now = day::bounds(Date::new(2026, 6, 15).expect("date")).0;
        let old = now - 100 * 86_400;
        let old_day = day::key(day::date_of(old));
        fixture
            .store
            .insert_check("opac", old, &down())
            .await
            .expect("old check");
        fixture
            .store
            .insert_check("opac", now, &up())
            .await
            .expect("new check");
        // A rollup far outside the retention window.
        fixture
            .store
            .upsert_daily_stat(
                "opac",
                &DailyStat {
                    day: "2019-01-05".to_string(),
                    checks: 1,
                    ok_checks: 1,
                    down_seconds: 0,
                    avg_latency_ms: Some(10),
                    max_latency_ms: Some(10),
                },
            )
            .await
            .expect("old rollup");

        let report = pass(&fixture.store, now).await.expect("pass");
        assert_eq!(report.pruned_checks, 1);
        assert_eq!(report.pruned_days, 1);
        assert_eq!(raw_checks(&fixture.path), 1);

        let rows = fixture
            .store
            .daily_stats("opac", "2019-01-01")
            .await
            .expect("stats");
        let days: Vec<&str> = rows.iter().map(|row| row.day.as_str()).collect();
        assert_eq!(days, vec![old_day.as_str(), "2026-06-15"]);
        assert_eq!(rows[0].checks, 1);
        assert_eq!(rows[0].down_seconds, 300);
    }

    #[test]
    fn a_check_covers_its_interval_but_not_a_gap() {
        let day = Date::new(2026, 6, 15).expect("date");
        let start = day::bounds(day).0;
        // Two failures five minutes apart, then a two-hour hole, then one more.
        let checks = vec![
            (start, false, Some(10_000)),
            (start + 300, false, Some(10_000)),
            (start + 7200, false, Some(10_000)),
        ];
        let stat = day_stat(day, &checks, 300);
        // The three failures are five minutes each; the hole counts as nothing.
        assert_eq!(stat.down_seconds, 3 * 300);
        assert_eq!(stat.checks, 3);
    }

    #[test]
    fn checks_outside_the_day_are_ignored() {
        let day = Date::new(2026, 6, 15).expect("date");
        let (start, end) = day::bounds(day);
        let checks = vec![
            (start - 300, false, None),
            (start, true, Some(50)),
            (end, false, None),
        ];
        let stat = day_stat(day, &checks, 300);
        assert_eq!(stat.checks, 1);
        assert_eq!(stat.ok_checks, 1);
        assert_eq!(stat.down_seconds, 0);
        assert_eq!(stat.avg_latency_ms, Some(50));
    }
}
