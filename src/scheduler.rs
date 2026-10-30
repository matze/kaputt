//! Per-target probing loop and the incident state machine.
//!
//! The bookkeeping lives in the database, not in memory: every check first
//! derives the trailing run of equal results and the open incident from the
//! stored rows, then decides whether a threshold was crossed. A restart
//! therefore resumes exactly where the last stored check left off, and an
//! incident cannot be duplicated by it.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::task::JoinHandle;

use crate::config::{Config, TargetConfig};
use crate::db::{self, RunHead, Store, TargetHistory};
use crate::probe::{Outcome, Prober};

/// Jitter added to the interval, in percent: checks drift by ±5 %.
const JITTER_PERCENT: u64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunKind {
    Ok,
    Fail,
}

impl RunKind {
    fn of(ok: bool) -> Self {
        if ok { Self::Ok } else { Self::Fail }
    }
}

/// Trailing run of checks with the same result, grown one check at a time.
#[derive(Debug, Clone)]
struct Run {
    kind: RunKind,
    len: u32,
    /// Time of the first check in the run; an incident starts and ends there.
    started_at: i64,
    /// Error of the first failing check; unused for a successful run.
    cause: String,
    detail: Option<String>,
}

/// The database write a crossed threshold asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    Open {
        started_at: i64,
        cause: String,
        detail: Option<String>,
    },
    Close {
        incident_id: i64,
        ended_at: i64,
    },
}

/// Hysteresis and incident bookkeeping for a single target.
#[derive(Debug, Clone)]
struct Tracker {
    state: State,
    run: Option<Run>,
    open_incident: Option<i64>,
    fail_threshold: u32,
    ok_threshold: u32,
}

impl Tracker {
    /// Rebuild the state from the stored history. A target without history
    /// starts as up: nothing has failed yet.
    fn from_history(history: TargetHistory, target: &TargetConfig) -> Self {
        let run = history.run_head.map(
            |RunHead {
                 ok,
                 ts,
                 error_kind,
                 error_detail,
             }| Run {
                kind: RunKind::of(ok),
                len: history.run_len,
                started_at: ts,
                cause: error_kind.unwrap_or_else(|| "other".to_string()),
                detail: error_detail,
            },
        );
        Self {
            state: if history.open_incident.is_some() {
                State::Down
            } else {
                State::Up
            },
            run,
            open_incident: history.open_incident,
            fail_threshold: target.fail_threshold,
            ok_threshold: target.ok_threshold,
        }
    }

    /// Open the incident a failing run already earned but that no process
    /// wrote: checks stored by a crash, by an older binary, or by
    /// `probe --once` while the scheduler was not running.
    fn reconcile(&mut self) -> Option<Action> {
        if self.open_incident.is_some() {
            return None;
        }
        let run = self.run.as_ref()?;
        if run.kind != RunKind::Fail || run.len < self.fail_threshold {
            return None;
        }
        self.state = State::Down;
        Some(Action::Open {
            started_at: run.started_at,
            cause: run.cause.clone(),
            detail: run.detail.clone(),
        })
    }

    /// Fold one check into the state and report the incident change, if any.
    fn observe(&mut self, ts: i64, outcome: &Outcome) -> Option<Action> {
        let kind = RunKind::of(outcome.ok);
        match &mut self.run {
            Some(run) if run.kind == kind => run.len += 1,
            _ => {
                self.run = Some(Run {
                    kind,
                    len: 1,
                    started_at: ts,
                    cause: outcome
                        .error_kind
                        .map(|kind| kind.as_str().to_string())
                        .unwrap_or_else(|| "other".to_string()),
                    detail: outcome.error_detail.clone(),
                });
            }
        }
        let run = self.run.as_ref().expect("run was just set");

        match kind {
            RunKind::Ok if self.state == State::Down && run.len >= self.ok_threshold => {
                self.state = State::Up;
                // The site answered at the first success of the recovering
                // run; the incident ends there, not at the threshold check.
                self.open_incident.take().map(|incident_id| Action::Close {
                    incident_id,
                    ended_at: run.started_at,
                })
            }
            RunKind::Fail if self.state == State::Up && run.len >= self.fail_threshold => {
                self.state = State::Down;
                Some(Action::Open {
                    started_at: run.started_at,
                    cause: run.cause.clone(),
                    detail: run.detail.clone(),
                })
            }
            _ => None,
        }
    }
}

/// Store one check and apply the incident change it triggers.
async fn record(
    store: &Store,
    target: &TargetConfig,
    ts: i64,
    outcome: &Outcome,
) -> anyhow::Result<()> {
    let mut tracker = Tracker::from_history(store.target_history(&target.id).await?, target);
    if let Some(action) = tracker.reconcile() {
        apply(store, target, &mut tracker, action).await?;
    }
    store.insert_check(&target.id, ts, outcome).await?;
    if let Some(action) = tracker.observe(ts, outcome) {
        apply(store, target, &mut tracker, action).await?;
    }
    Ok(())
}

async fn apply(
    store: &Store,
    target: &TargetConfig,
    tracker: &mut Tracker,
    action: Action,
) -> anyhow::Result<()> {
    match action {
        Action::Open {
            started_at,
            cause,
            detail,
        } => {
            let id = store
                .open_incident(&target.id, started_at, &cause, detail)
                .await?;
            tracker.open_incident = Some(id);
            tracing::warn!(
                target = %target.id,
                started_at,
                cause = %cause,
                "incident opened"
            );
        }
        Action::Close {
            incident_id,
            ended_at,
        } => {
            store.close_incident(incident_id, ended_at).await?;
            tracker.open_incident = None;
            tracing::info!(target = %target.id, incident_id, ended_at, "incident closed");
        }
    }
    Ok(())
}

/// One probe plus its storage and incident bookkeeping.
pub async fn check_once(
    store: &Store,
    prober: &Prober,
    target: &TargetConfig,
) -> anyhow::Result<Outcome> {
    let outcome = prober.probe(target).await;
    record(store, target, db::unix_now(), &outcome).await?;
    Ok(outcome)
}

/// Start one loop per enabled target: probe immediately, then every
/// `interval_s` with jitter. The scheduler does not hold state, so a target
/// that fails for a while keeps counting across restarts.
pub fn spawn(config: &Config, store: &Store) -> anyhow::Result<Vec<JoinHandle<()>>> {
    let targets: Vec<TargetConfig> = config
        .targets
        .iter()
        .filter(|target| target.enabled)
        .cloned()
        .collect();
    if targets.is_empty() {
        return Ok(Vec::new());
    }

    let prober = Arc::new(Prober::new()?);
    let store = store.clone();
    Ok(targets
        .into_iter()
        .map(|target| {
            let store = store.clone();
            let prober = Arc::clone(&prober);
            tokio::spawn(async move {
                let mut jitter = Jitter::seeded(&target.id);
                loop {
                    if let Err(err) = check_once(&store, &prober, &target).await {
                        tracing::warn!(target = %target.id, %err, "check could not be recorded");
                    }
                    tokio::time::sleep(jitter.interval(target.interval_s)).await;
                }
            })
        })
        .collect())
}

/// Small xorshift PRNG used only to spread probe times; not security relevant.
struct Jitter {
    state: u64,
}

impl Jitter {
    fn seeded(target_id: &str) -> Self {
        // FNV-1a of the target id, mixed with the clock so a restart does not
        // put every target back on the same cadence.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in target_id.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        let clock = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0);
        Self {
            state: hash ^ clock | 1,
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// `interval_s` shifted by up to ±[`JITTER_PERCENT`].
    fn interval(&mut self, interval_s: u32) -> Duration {
        let base_ms = u64::from(interval_s.max(1)) * 1000;
        let spread = base_ms * JITTER_PERCENT / 100;
        let offset = if spread == 0 {
            0
        } else {
            self.next_u64() % (2 * spread + 1)
        };
        Duration::from_millis(base_ms - spread + offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::ErrorKind;
    use std::path::{Path, PathBuf};
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
            latency_ms: Some(42),
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
        target: TargetConfig,
    }

    async fn fixture() -> Fixture {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("kaputt.db");
        let store = Store::open(&path).await.expect("store opens");
        let target = target();
        store
            .upsert_targets(std::slice::from_ref(&target))
            .await
            .expect("upsert");
        Fixture {
            _dir: dir,
            path,
            store,
            target,
        }
    }

    /// Incidents of the test target as (started_at, ended_at, cause).
    fn incidents(path: &Path) -> Vec<(i64, Option<i64>, String)> {
        let conn = rusqlite::Connection::open(path).expect("raw open");
        let mut stmt = conn
            .prepare("SELECT started_at, ended_at, cause FROM incidents ORDER BY started_at")
            .expect("prepare");
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows")
    }

    async fn run(fixture: &Fixture, checks: &[(i64, Outcome)]) {
        for (ts, outcome) in checks {
            record(&fixture.store, &fixture.target, *ts, outcome)
                .await
                .expect("record");
        }
    }

    #[tokio::test]
    async fn failures_open_one_incident_at_the_first_failure() {
        let fixture = fixture().await;
        run(
            &fixture,
            &[(1000, up()), (1100, down()), (1200, down()), (1300, down())],
        )
        .await;
        assert_eq!(
            incidents(&fixture.path),
            vec![(1100, None, "timeout".to_string())]
        );
    }

    #[tokio::test]
    async fn a_single_failure_is_not_an_incident() {
        let fixture = fixture().await;
        run(&fixture, &[(1000, up()), (1100, down()), (1200, up())]).await;
        assert!(incidents(&fixture.path).is_empty());
    }

    #[tokio::test]
    async fn recovery_needs_the_ok_threshold_and_closes_at_the_first_success() {
        let fixture = fixture().await;
        run(
            &fixture,
            &[(1000, up()), (1100, down()), (1200, down()), (1300, up())],
        )
        .await;
        assert_eq!(
            incidents(&fixture.path),
            vec![(1100, None, "timeout".to_string())]
        );

        run(&fixture, &[(1400, up())]).await;
        assert_eq!(
            incidents(&fixture.path),
            vec![(1100, Some(1300), "timeout".to_string())]
        );
    }

    #[tokio::test]
    async fn flapping_inside_an_incident_does_not_multiply_incidents() {
        let fixture = fixture().await;
        run(&fixture, &[(1000, up()), (1100, down()), (1200, down())]).await;
        run(&fixture, &[(1300, up()), (1400, down()), (1500, up())]).await;
        assert_eq!(
            incidents(&fixture.path),
            vec![(1100, None, "timeout".to_string())]
        );

        run(&fixture, &[(1600, up())]).await;
        assert_eq!(
            incidents(&fixture.path),
            vec![(1100, Some(1500), "timeout".to_string())]
        );
    }

    #[tokio::test]
    async fn a_restart_with_an_open_incident_does_not_open_a_second() {
        let fixture = fixture().await;
        run(&fixture, &[(1000, up()), (1100, down()), (1200, down())]).await;
        drop(fixture.store);

        let store = Store::open(&fixture.path).await.expect("reopen");
        for (ts, outcome) in [(1300, down()), (1400, down()), (1500, down())] {
            record(&store, &fixture.target, ts, &outcome)
                .await
                .expect("record");
        }
        assert_eq!(
            incidents(&fixture.path),
            vec![(1100, None, "timeout".to_string())]
        );
    }

    #[tokio::test]
    async fn a_failure_before_a_restart_still_counts_towards_the_threshold() {
        let fixture = fixture().await;
        run(&fixture, &[(1000, up()), (1100, down())]).await;
        drop(fixture.store);

        let store = Store::open(&fixture.path).await.expect("reopen");
        record(&store, &fixture.target, 1200, &down())
            .await
            .expect("record");
        assert_eq!(
            incidents(&fixture.path),
            vec![(1100, None, "timeout".to_string())]
        );
    }

    #[tokio::test]
    async fn failing_checks_without_an_incident_are_repaired() {
        let fixture = fixture().await;
        fixture
            .store
            .insert_check("opac", 1000, &down())
            .await
            .expect("insert");
        fixture
            .store
            .insert_check("opac", 1100, &down())
            .await
            .expect("insert");
        assert!(incidents(&fixture.path).is_empty());

        run(&fixture, &[(1200, down())]).await;
        assert_eq!(
            incidents(&fixture.path),
            vec![(1000, None, "timeout".to_string())]
        );
    }

    #[tokio::test]
    async fn a_recovery_stored_while_down_still_closes_the_incident() {
        let fixture = fixture().await;
        run(&fixture, &[(1000, up()), (1100, down()), (1200, down())]).await;
        // Crash before the recovery could be processed: the successes landed
        // in the database without a state machine seeing them.
        fixture
            .store
            .insert_check("opac", 1300, &up())
            .await
            .expect("insert");
        fixture
            .store
            .insert_check("opac", 1400, &up())
            .await
            .expect("insert");

        run(&fixture, &[(1500, up())]).await;
        assert_eq!(
            incidents(&fixture.path),
            vec![(1100, Some(1300), "timeout".to_string())]
        );
    }

    #[tokio::test]
    async fn a_new_incident_records_the_error_of_its_first_check() {
        let fixture = fixture().await;
        let mut status_down = down();
        status_down.error_kind = Some(ErrorKind::Status);
        status_down.status_code = Some(503);
        status_down.error_detail = Some("HTTP 503, erwartet 200".into());
        run(
            &fixture,
            &[
                (1000, up()),
                (1100, status_down.clone()),
                (1200, status_down),
            ],
        )
        .await;

        let incident = &incidents(&fixture.path)[0];
        assert_eq!(incident.2, "status");

        let conn = rusqlite::Connection::open(&fixture.path).expect("raw open");
        let detail: String = conn
            .query_row("SELECT detail FROM incidents", [], |row| row.get(0))
            .expect("detail");
        assert_eq!(detail, "HTTP 503, erwartet 200");
    }

    #[test]
    fn jitter_stays_within_five_percent() {
        let mut jitter = Jitter::seeded("opac");
        let samples: Vec<Duration> = (0..64).map(|_| jitter.interval(300)).collect();
        for sample in &samples {
            assert!(
                *sample >= Duration::from_millis(285_000)
                    && *sample <= Duration::from_millis(315_000),
                "{sample:?}"
            );
        }
        assert!(
            samples.windows(2).any(|pair| pair[0] != pair[1]),
            "jitter never changes: {samples:?}"
        );
    }

    #[test]
    fn jitter_keeps_short_intervals_positive() {
        let mut jitter = Jitter::seeded("opac");
        for _ in 0..64 {
            let sample = jitter.interval(1);
            assert!(sample >= Duration::from_millis(900), "{sample:?}");
            assert!(sample <= Duration::from_millis(1100), "{sample:?}");
        }
    }
}
