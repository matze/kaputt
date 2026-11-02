//! SQLite storage: schema migrations, target sync, check inserts.
//!
//! One writer connection shared through `tokio-rusqlite`; every operation runs
//! on that connection's dedicated thread, so no lock handling leaks into the
//! async code.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use rusqlite::OptionalExtension;
use tokio_rusqlite::Connection;

use crate::config::TargetConfig;
use crate::probe::Outcome;

/// Ordered schema migrations; step `n` moves `PRAGMA user_version` to `n + 1`.
const MIGRATIONS: &[&str] = &[SCHEMA_V1, SCHEMA_V2];

const SCHEMA_V1: &str = r#"
CREATE TABLE targets (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    url TEXT NOT NULL,
    expect_status INTEGER NOT NULL DEFAULT 200,
    interval_s INTEGER NOT NULL,
    timeout_s INTEGER NOT NULL,
    fail_threshold INTEGER NOT NULL DEFAULT 2,
    ok_threshold INTEGER NOT NULL DEFAULT 2,
    enabled INTEGER NOT NULL DEFAULT 1,
    sort INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE TABLE checks (
    id INTEGER PRIMARY KEY,
    target_id TEXT NOT NULL REFERENCES targets(id) ON DELETE CASCADE,
    ts INTEGER NOT NULL,
    ok INTEGER NOT NULL,
    status_code INTEGER,
    latency_ms INTEGER,
    error_kind TEXT,
    error_detail TEXT
) STRICT;

CREATE INDEX checks_target_ts ON checks (target_id, ts);

CREATE TABLE incidents (
    id INTEGER PRIMARY KEY,
    target_id TEXT NOT NULL REFERENCES targets(id) ON DELETE CASCADE,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    cause TEXT NOT NULL,
    detail TEXT,
    CHECK (ended_at IS NULL OR ended_at >= started_at)
) STRICT;

CREATE INDEX incidents_target_started ON incidents (target_id, started_at);

CREATE TABLE daily_stats (
    target_id TEXT NOT NULL REFERENCES targets(id) ON DELETE CASCADE,
    day TEXT NOT NULL,
    checks INTEGER NOT NULL,
    ok_checks INTEGER NOT NULL,
    down_seconds INTEGER NOT NULL,
    avg_latency_ms INTEGER,
    max_latency_ms INTEGER,
    PRIMARY KEY (target_id, day)
) STRICT;
"#;

/// One incident per target can be open at a time; the partial unique index
/// makes a duplicated incident a write error instead of a silent data bug.
const SCHEMA_V2: &str = r#"
CREATE UNIQUE INDEX incidents_open_per_target ON incidents (target_id)
    WHERE ended_at IS NULL;
"#;

/// Incident bookkeeping for one target, derived from the stored rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetHistory {
    /// Id of the incident still running, if any.
    pub open_incident: Option<i64>,
    /// First check of the trailing run of equal results, `None` when the
    /// target has no checks yet.
    pub run_head: Option<RunHead>,
    /// Number of checks in that trailing run.
    pub run_len: u32,
}

/// First check of the trailing run: its time and, for a failing run, the error
/// that opened it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunHead {
    pub ok: bool,
    pub ts: i64,
    pub error_kind: Option<String>,
    pub error_detail: Option<String>,
}

#[derive(Clone)]
pub struct Store {
    conn: Connection,
}

impl Store {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }

        let conn = Connection::open(path)
            .await
            .with_context(|| format!("cannot open {}", path.display()))?;

        conn.call(|conn| {
            conn.execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 PRAGMA foreign_keys = ON;
                 PRAGMA busy_timeout = 5000;",
            )?;
            migrate(conn)
        })
        .await
        .with_context(|| format!("cannot migrate {}", path.display()))?;

        Ok(Self { conn })
    }

    /// Write every configured target, updating existing rows. Targets that
    /// disappeared from the config are disabled, never deleted, so their
    /// history stays available.
    pub async fn upsert_targets(&self, targets: &[TargetConfig]) -> anyhow::Result<()> {
        let targets = targets.to_vec();
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                // Start from "everything disabled"; the upsert below re-enables
                // exactly the targets the config still lists.
                tx.execute("UPDATE targets SET enabled = 0", [])?;
                {
                    let mut upsert = tx.prepare(
                        "INSERT INTO targets (
                             id, name, url, expect_status, interval_s, timeout_s,
                             fail_threshold, ok_threshold, enabled, sort
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                         ON CONFLICT (id) DO UPDATE SET
                             name = excluded.name,
                             url = excluded.url,
                             expect_status = excluded.expect_status,
                             interval_s = excluded.interval_s,
                             timeout_s = excluded.timeout_s,
                             fail_threshold = excluded.fail_threshold,
                             ok_threshold = excluded.ok_threshold,
                             enabled = excluded.enabled,
                             sort = excluded.sort",
                    )?;
                    for target in &targets {
                        upsert.execute(rusqlite::params![
                            target.id,
                            target.name,
                            target.url,
                            target.expect_status,
                            target.interval_s,
                            target.timeout_s,
                            target.fail_threshold,
                            target.ok_threshold,
                            target.enabled as i64,
                            target.sort,
                        ])?;
                    }
                }
                tx.commit()?;
                Ok::<(), rusqlite::Error>(())
            })
            .await?;
        Ok(())
    }

    pub async fn insert_check(
        &self,
        target_id: &str,
        ts: i64,
        outcome: &Outcome,
    ) -> anyhow::Result<i64> {
        let target_id = target_id.to_string();
        let outcome = outcome.clone();
        let id = self
            .conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO checks (target_id, ts, ok, status_code, latency_ms, error_kind, error_detail)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        target_id,
                        ts,
                        outcome.ok as i64,
                        outcome.status_code,
                        outcome.latency_ms,
                        outcome.error_kind.map(|k| k.as_str()),
                        outcome.error_detail,
                    ],
                )?;
                Ok::<i64, rusqlite::Error>(conn.last_insert_rowid())
            })
            .await?;
        Ok(id)
    }

    /// What the incident state machine needs to resume: the open incident and
    /// the trailing run of equal check results. Row ids order the checks
    /// because they are inserted in check order; two checks inside the same
    /// second keep their sequence that way.
    pub async fn target_history(&self, target_id: &str) -> anyhow::Result<TargetHistory> {
        let target_id = target_id.to_string();
        let history = self
            .conn
            .call(move |conn| {
                let open_incident = conn
                    .query_row(
                        "SELECT id FROM incidents
                         WHERE target_id = ?1 AND ended_at IS NULL
                         ORDER BY started_at DESC, id DESC LIMIT 1",
                        [&target_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()?;

                let newest_ok = conn
                    .query_row(
                        "SELECT ok FROM checks WHERE target_id = ?1
                         ORDER BY id DESC LIMIT 1",
                        [&target_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()?;
                let Some(newest_ok) = newest_ok else {
                    return Ok::<_, rusqlite::Error>(TargetHistory {
                        open_incident,
                        run_head: None,
                        run_len: 0,
                    });
                };

                // Everything after the newest check with the other result
                // belongs to the trailing run.
                let boundary: i64 = conn
                    .query_row(
                        "SELECT id FROM checks WHERE target_id = ?1 AND ok != ?2
                         ORDER BY id DESC LIMIT 1",
                        rusqlite::params![target_id, newest_ok],
                        |row| row.get(0),
                    )
                    .optional()?
                    .unwrap_or(0);

                let run_len: i64 = conn.query_row(
                    "SELECT count(*) FROM checks WHERE target_id = ?1 AND id > ?2",
                    rusqlite::params![target_id, boundary],
                    |row| row.get(0),
                )?;
                let (ts, error_kind, error_detail) = conn.query_row(
                    "SELECT ts, error_kind, error_detail FROM checks WHERE id =
                         (SELECT min(id) FROM checks WHERE target_id = ?1 AND id > ?2)",
                    rusqlite::params![target_id, boundary],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    },
                )?;

                Ok(TargetHistory {
                    open_incident,
                    run_head: Some(RunHead {
                        ok: newest_ok != 0,
                        ts,
                        error_kind,
                        error_detail,
                    }),
                    run_len: run_len as u32,
                })
            })
            .await?;
        Ok(history)
    }

    /// Insert an incident that starts at `started_at`. The schema allows only
    /// one open incident per target, so a duplicate attempt fails here.
    pub async fn open_incident(
        &self,
        target_id: &str,
        started_at: i64,
        cause: &str,
        detail: Option<String>,
    ) -> anyhow::Result<i64> {
        let target_id = target_id.to_string();
        let cause = cause.to_string();
        let id = self
            .conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO incidents (target_id, started_at, cause, detail)
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![target_id, started_at, cause, detail],
                )?;
                Ok::<i64, rusqlite::Error>(conn.last_insert_rowid())
            })
            .await?;
        Ok(id)
    }

    /// End an open incident. Closing an already closed incident is reported as
    /// an error: the caller derives its state from the database and repairs
    /// itself on the next check.
    pub async fn close_incident(&self, incident_id: i64, ended_at: i64) -> anyhow::Result<()> {
        let closed = self
            .conn
            .call(move |conn| {
                conn.execute(
                    "UPDATE incidents SET ended_at = ?2 WHERE id = ?1 AND ended_at IS NULL",
                    rusqlite::params![incident_id, ended_at],
                )
            })
            .await?;
        anyhow::ensure!(closed == 1, "incident {incident_id} was not open");
        Ok(())
    }
}

/// One `checks` row as the pages show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckSummary {
    pub ts: i64,
    pub ok: bool,
    pub status_code: Option<u16>,
    pub latency_ms: Option<i64>,
    pub error_kind: Option<String>,
    pub error_detail: Option<String>,
}

/// One `incidents` row; `ended_at` is `None` while the incident runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncidentSummary {
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub cause: String,
    pub detail: Option<String>,
}

/// One incident with the identity a feed item needs: the row id for a stable
/// GUID, and the target it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedIncident {
    pub id: i64,
    pub target_id: String,
    pub target_name: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub cause: String,
    pub detail: Option<String>,
}

/// A target plus what the pages need from its history: newest check, newest
/// successful check, and the incident still open (if any).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetState {
    pub id: String,
    pub name: String,
    pub url: String,
    pub expect_status: u16,
    pub interval_s: u32,
    pub last_check: Option<CheckSummary>,
    pub last_ok_ts: Option<i64>,
    pub open_incident: Option<IncidentSummary>,
}

/// `ok` flag and timestamps of the checks since `since`, oldest first.
pub type HistoryPoint = (i64, bool);

/// One check in a chart window, oldest first: timestamp, success flag and the
/// latency when the probe measured one. A failed status check has a latency
/// but no usable answer time, so the chart marks it instead of plotting it.
pub type ChartCheck = (i64, bool, Option<i64>);

/// One `daily_stats` row: a local day of one target, as the rollup stores it.
/// `day` is the `YYYY-MM-DD` key from [`crate::day::key`], so text order is
/// day order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyStat {
    pub day: String,
    pub checks: u32,
    pub ok_checks: u32,
    pub down_seconds: i64,
    pub avg_latency_ms: Option<i64>,
    pub max_latency_ms: Option<i64>,
}

impl Store {
    /// Enabled targets in display order, each with its newest state.
    pub async fn enabled_targets(&self) -> anyhow::Result<Vec<TargetState>> {
        self.target_states(None, true).await
    }

    /// One target by id, enabled or not: a disabled target keeps its history
    /// readable.
    pub async fn target(&self, id: &str) -> anyhow::Result<Option<TargetState>> {
        Ok(self
            .target_states(Some(id.to_string()), false)
            .await?
            .into_iter()
            .next())
    }

    async fn target_states(
        &self,
        only_id: Option<String>,
        enabled: bool,
    ) -> anyhow::Result<Vec<TargetState>> {
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT t.id, t.name, t.url, t.expect_status, t.interval_s,
                            c.ts, c.ok, c.status_code, c.latency_ms, c.error_kind, c.error_detail,
                            (SELECT max(ts) FROM checks WHERE target_id = t.id AND ok = 1),
                            i.id, i.started_at, i.cause, i.detail
                     FROM targets t
                     LEFT JOIN checks c ON c.id =
                         (SELECT max(id) FROM checks WHERE target_id = t.id)
                     LEFT JOIN incidents i ON i.id =
                         (SELECT id FROM incidents
                          WHERE target_id = t.id AND ended_at IS NULL
                          ORDER BY started_at DESC, id DESC LIMIT 1)
                     WHERE (?1 IS NULL OR t.id = ?1) AND (?2 = 0 OR t.enabled = 1)
                     ORDER BY t.sort, t.id",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![only_id, enabled as i64], |row| {
                        let last_check = match row.get::<_, Option<i64>>(5)? {
                            Some(ts) => Some(CheckSummary {
                                ts,
                                ok: row.get::<_, i64>(6)? != 0,
                                status_code: row.get::<_, Option<i64>>(7)?.map(|c| c as u16),
                                latency_ms: row.get::<_, Option<i64>>(8)?,
                                error_kind: row.get(9)?,
                                error_detail: row.get(10)?,
                            }),
                            None => None,
                        };
                        let last_ok_ts = row.get::<_, Option<i64>>(11)?;
                        let open_incident = match row.get::<_, Option<i64>>(12)? {
                            Some(_) => Some(IncidentSummary {
                                started_at: row.get::<_, i64>(13)?,
                                ended_at: None,
                                cause: row.get::<_, String>(14)?,
                                detail: row.get(15)?,
                            }),
                            None => None,
                        };
                        Ok(TargetState {
                            id: row.get(0)?,
                            name: row.get(1)?,
                            url: row.get(2)?,
                            expect_status: row.get::<_, i64>(3)? as u16,
                            interval_s: row.get::<_, i64>(4)? as u32,
                            last_check,
                            last_ok_ts,
                            open_incident,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, rusqlite::Error>(rows)
            })
            .await
            .map_err(Into::into)
    }

    /// Every check of one target from `since` on, oldest first.
    pub async fn history_since(
        &self,
        target_id: &str,
        since: i64,
    ) -> anyhow::Result<Vec<HistoryPoint>> {
        let target_id = target_id.to_string();
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT ts, ok FROM checks WHERE target_id = ?1 AND ts >= ?2 ORDER BY ts, id",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![target_id, since], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)? != 0))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, rusqlite::Error>(rows)
            })
            .await
            .map_err(Into::into)
    }

    /// Every check of one target from `since` on, oldest first, for the chart.
    pub async fn chart_since(
        &self,
        target_id: &str,
        since: i64,
    ) -> anyhow::Result<Vec<ChartCheck>> {
        let target_id = target_id.to_string();
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT ts, ok, latency_ms FROM checks
                     WHERE target_id = ?1 AND ts >= ?2 ORDER BY ts, id",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![target_id, since], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)? != 0,
                            row.get::<_, Option<i64>>(2)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, rusqlite::Error>(rows)
            })
            .await
            .map_err(Into::into)
    }

    /// Newest incidents of one target, newest first.
    pub async fn recent_incidents(
        &self,
        target_id: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<IncidentSummary>> {
        let target_id = target_id.to_string();
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT started_at, ended_at, cause, detail FROM incidents
                     WHERE target_id = ?1 ORDER BY started_at DESC, id DESC LIMIT ?2",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![target_id, limit], |row| {
                        Ok(IncidentSummary {
                            started_at: row.get(0)?,
                            ended_at: row.get(1)?,
                            cause: row.get(2)?,
                            detail: row.get(3)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, rusqlite::Error>(rows)
            })
            .await
            .map_err(Into::into)
    }

    /// Incidents a feed can show: everything of `target_id` (or every enabled
    /// target when `None`) that ended at or after `since`. An incident still
    /// running is always included, however old it is.
    pub async fn feed_incidents(
        &self,
        target_id: Option<&str>,
        since: i64,
        enabled_only: bool,
    ) -> anyhow::Result<Vec<FeedIncident>> {
        let target_id = target_id.map(str::to_string);
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT i.id, i.target_id, t.name, i.started_at, i.ended_at,
                            i.cause, i.detail
                     FROM incidents i JOIN targets t ON t.id = i.target_id
                     WHERE (?1 IS NULL OR i.target_id = ?1)
                       AND (i.ended_at IS NULL OR i.ended_at >= ?2)
                       AND (?3 = 0 OR t.enabled = 1)
                     ORDER BY i.target_id, i.started_at, i.id",
                )?;
                let rows = stmt
                    .query_map(
                        rusqlite::params![target_id, since, enabled_only as i64],
                        |row| {
                            Ok(FeedIncident {
                                id: row.get(0)?,
                                target_id: row.get(1)?,
                                target_name: row.get(2)?,
                                started_at: row.get(3)?,
                                ended_at: row.get(4)?,
                                cause: row.get(5)?,
                                detail: row.get(6)?,
                            })
                        },
                    )?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, rusqlite::Error>(rows)
            })
            .await
            .map_err(Into::into)
    }
}

/// Daily rollups and the retention that bounds both tables.
impl Store {
    /// Every target with the interval a rollup needs, enabled or not: the
    /// history of a target dropped from the config keeps being rolled up.
    pub async fn rollup_targets(&self) -> anyhow::Result<Vec<(String, i64)>> {
        self.conn
            .call(|conn| {
                let mut stmt = conn.prepare("SELECT id, interval_s FROM targets ORDER BY id")?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, rusqlite::Error>(rows)
            })
            .await
            .map_err(Into::into)
    }

    /// Time of the oldest stored check; `None` when the target has none.
    pub async fn first_check_ts(&self, target_id: &str) -> anyhow::Result<Option<i64>> {
        let target_id = target_id.to_string();
        self.conn
            .call(move |conn| {
                let first = conn
                    .query_row(
                        "SELECT min(ts) FROM checks WHERE target_id = ?1",
                        [&target_id],
                        |row| row.get::<_, Option<i64>>(0),
                    )
                    .optional()?
                    .flatten();
                Ok::<_, rusqlite::Error>(first)
            })
            .await
            .map_err(Into::into)
    }

    /// Newest day the rollup wrote for a target, as a day key.
    pub async fn last_rolled_day(&self, target_id: &str) -> anyhow::Result<Option<String>> {
        let target_id = target_id.to_string();
        self.conn
            .call(move |conn| {
                let day = conn
                    .query_row(
                        "SELECT max(day) FROM daily_stats WHERE target_id = ?1",
                        [&target_id],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .optional()?
                    .flatten();
                Ok::<_, rusqlite::Error>(day)
            })
            .await
            .map_err(Into::into)
    }

    /// Checks of one local day, oldest first: `(ts, ok, latency_ms)`.
    pub async fn checks_between(
        &self,
        target_id: &str,
        start: i64,
        end: i64,
    ) -> anyhow::Result<Vec<(i64, bool, Option<i64>)>> {
        let target_id = target_id.to_string();
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT ts, ok, latency_ms FROM checks
                     WHERE target_id = ?1 AND ts >= ?2 AND ts < ?3 ORDER BY ts, id",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![target_id, start, end], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)? != 0,
                            row.get::<_, Option<i64>>(2)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, rusqlite::Error>(rows)
            })
            .await
            .map_err(Into::into)
    }

    /// Write one day of rollup numbers, replacing what was there: a day that
    /// was rolled while it was still running gets corrected on the next pass.
    pub async fn upsert_daily_stat(&self, target_id: &str, stat: &DailyStat) -> anyhow::Result<()> {
        let target_id = target_id.to_string();
        let stat = stat.clone();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO daily_stats (
                         target_id, day, checks, ok_checks, down_seconds,
                         avg_latency_ms, max_latency_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT (target_id, day) DO UPDATE SET
                         checks = excluded.checks,
                         ok_checks = excluded.ok_checks,
                         down_seconds = excluded.down_seconds,
                         avg_latency_ms = excluded.avg_latency_ms,
                         max_latency_ms = excluded.max_latency_ms",
                    rusqlite::params![
                        target_id,
                        stat.day,
                        stat.checks,
                        stat.ok_checks,
                        stat.down_seconds,
                        stat.avg_latency_ms,
                        stat.max_latency_ms,
                    ],
                )?;
                Ok::<(), rusqlite::Error>(())
            })
            .await?;
        Ok(())
    }

    /// Rollups of one target from `from_day` on, oldest first.
    pub async fn daily_stats(
        &self,
        target_id: &str,
        from_day: &str,
    ) -> anyhow::Result<Vec<DailyStat>> {
        let target_id = target_id.to_string();
        let from_day = from_day.to_string();
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT day, checks, ok_checks, down_seconds, avg_latency_ms, max_latency_ms
                     FROM daily_stats WHERE target_id = ?1 AND day >= ?2 ORDER BY day",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![target_id, from_day], |row| {
                        Ok(DailyStat {
                            day: row.get(0)?,
                            checks: row.get::<_, i64>(1)? as u32,
                            ok_checks: row.get::<_, i64>(2)? as u32,
                            down_seconds: row.get(3)?,
                            avg_latency_ms: row.get(4)?,
                            max_latency_ms: row.get(5)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, rusqlite::Error>(rows)
            })
            .await
            .map_err(Into::into)
    }

    /// Delete raw checks older than `before_ts`; returns the rows deleted.
    pub async fn prune_checks(&self, before_ts: i64) -> anyhow::Result<usize> {
        self.conn
            .call(move |conn| {
                let deleted = conn.execute("DELETE FROM checks WHERE ts < ?1", [before_ts])?;
                Ok::<usize, rusqlite::Error>(deleted)
            })
            .await
            .map_err(Into::into)
    }

    /// Delete rollups older than `before_day`; returns the rows deleted.
    pub async fn prune_daily_stats(&self, before_day: &str) -> anyhow::Result<usize> {
        let before_day = before_day.to_string();
        self.conn
            .call(move |conn| {
                let deleted =
                    conn.execute("DELETE FROM daily_stats WHERE day < ?1", [before_day])?;
                Ok::<usize, rusqlite::Error>(deleted)
            })
            .await
            .map_err(Into::into)
    }
}

/// Current wall-clock time as Unix seconds.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn migrate(conn: &mut rusqlite::Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let version = version.max(0) as usize;
    if version > MIGRATIONS.len() {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "database schema version {version} is newer than this binary ({} migrations)",
            MIGRATIONS.len()
        )));
    }

    for (index, sql) in MIGRATIONS.iter().enumerate().skip(version) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (index + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
