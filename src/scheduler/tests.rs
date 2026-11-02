use super::*;
use crate::probe::ErrorKind;
use futures_concurrency::future::Race;
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
            *sample >= Duration::from_millis(285_000) && *sample <= Duration::from_millis(315_000),
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

/// The server polls [`run`] inside its future tree: the first round must
/// record a check without a spawned task, and the future must stay pending so
/// the process keeps probing.
#[tokio::test]
async fn the_run_loop_probes_and_records_checks() {
    let fixture = fixture().await;
    let app = axum::Router::new().route("/ok", axum::routing::get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stub");
    let addr = listener.local_addr().expect("stub addr");
    let serve = async move {
        axum::serve(listener, app).await.expect("stub server");
    };

    let mut target = fixture.target.clone();
    target.url = format!("http://{addr}/ok");
    let targets = vec![target];
    let prober = Prober::new().expect("prober");

    let probes = super::run(&targets, &fixture.store, &prober);
    let recorded = async {
        loop {
            let history = fixture
                .store
                .history_since("opac", 0)
                .await
                .expect("history");
            if !history.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), (serve, probes, recorded).race())
        .await
        .expect("probe loop records a check");

    let history = fixture
        .store
        .history_since("opac", 0)
        .await
        .expect("history");
    assert_eq!(history.len(), 1);
    assert!(history[0].1);
}
