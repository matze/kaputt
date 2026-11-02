//! Per-target probing loop and the incident state machine.
//!
//! The bookkeeping lives in the database, not in memory: every check first
//! derives the trailing run of equal results and the open incident from the
//! stored rows, then decides whether a threshold was crossed. A restart
//! therefore resumes exactly where the last stored check left off, and an
//! incident cannot be duplicated by it.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_concurrency::future::Join;

use crate::config::TargetConfig;
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

/// One loop per enabled target: probe immediately, then every `interval_s`
/// with jitter. The caller supplies the [`Prober`] so building the HTTP client
/// stays a startup error, and joins this future with the endless rollup loop
/// so the process keeps serving even without targets. Dropping the future
/// cancels every loop.
pub async fn run(targets: &[TargetConfig], store: &Store, prober: &Prober) {
    let probes = targets
        .iter()
        .filter(|target| target.enabled)
        .map(|target| async move {
            let mut jitter = Jitter::seeded(&target.id);
            loop {
                if let Err(err) = check_once(store, prober, target).await {
                    tracing::warn!(target = %target.id, %err, "check could not be recorded");
                }
                tokio::time::sleep(jitter.interval(target.interval_s)).await;
            }
        });
    // An empty set is fine: the caller still joins this against the endless
    // rollup loop, so the process keeps serving without probes.
    let _: Vec<()> = probes.collect::<Vec<_>>().join().await;
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
mod tests;
