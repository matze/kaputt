use super::*;
use crate::probe::ErrorKind;
use std::path::PathBuf;
use tempfile::TempDir;

/// `checks` row as read back by the tests: ts, ok, status, latency, kind, detail.
type CheckRow = (
    i64,
    i64,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<String>,
);

fn target(id: &str, name: &str) -> TargetConfig {
    TargetConfig {
        id: id.into(),
        name: name.into(),
        url: format!("https://{id}.example.org/"),
        expect_status: 200,
        interval_s: 300,
        timeout_s: 10,
        fail_threshold: 2,
        ok_threshold: 2,
        enabled: true,
        sort: 0,
    }
}

fn success() -> Outcome {
    Outcome {
        ok: true,
        status_code: Some(200),
        latency_ms: Some(123),
        error_kind: None,
        error_detail: None,
    }
}

fn failure() -> Outcome {
    Outcome {
        ok: false,
        status_code: None,
        latency_ms: Some(10_000),
        error_kind: Some(ErrorKind::Timeout),
        error_detail: Some("operation timed out".into()),
    }
}

async fn open_temp() -> (TempDir, PathBuf, Store) {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("nested").join("kaputt.db");
    let store = Store::open(&path).await.expect("store opens");
    (dir, path, store)
}

fn schema_version(path: &Path) -> i64 {
    let conn = rusqlite::Connection::open(path).expect("raw open");
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("version")
}

fn table_exists(path: &Path, table: &str) -> bool {
    let conn = rusqlite::Connection::open(path).expect("raw open");
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .expect("count")
        == 1
}

#[tokio::test]
async fn open_creates_parent_directories_and_full_schema() {
    let (_dir, path, _store) = open_temp().await;
    assert_eq!(schema_version(&path), MIGRATIONS.len() as i64);
    for table in ["targets", "checks", "incidents", "daily_stats"] {
        assert!(table_exists(&path, table), "{table} missing");
    }
}

#[tokio::test]
async fn open_is_idempotent() {
    let (_dir, path, _store) = open_temp().await;
    let store = Store::open(&path).await.expect("second open");
    store
        .upsert_targets(&[target("opac", "OPAC")])
        .await
        .expect("upsert");
    assert_eq!(schema_version(&path), MIGRATIONS.len() as i64);
}

#[tokio::test]
async fn upsert_updates_in_place_and_disables_removed_targets() {
    let (_dir, path, store) = open_temp().await;
    store
        .upsert_targets(&[target("a", "Alt"), target("b", "Bleibt")])
        .await
        .expect("first upsert");
    store
        .upsert_targets(&[target("a", "Neu")])
        .await
        .expect("second upsert");

    let conn = rusqlite::Connection::open(&path).expect("raw open");
    let mut stmt = conn
        .prepare("SELECT id, name, enabled FROM targets ORDER BY id")
        .expect("prepare");
    let rows: Vec<(String, String, i64)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(
        rows,
        vec![
            ("a".to_string(), "Neu".to_string(), 1),
            ("b".to_string(), "Bleibt".to_string(), 0),
        ]
    );
}

#[tokio::test]
async fn insert_check_stores_success_and_failure() {
    let (_dir, path, store) = open_temp().await;
    store
        .upsert_targets(&[target("opac", "OPAC")])
        .await
        .expect("upsert");

    let ok_id = store
        .insert_check("opac", 1000, &success())
        .await
        .expect("insert");
    let fail_id = store
        .insert_check("opac", 1300, &failure())
        .await
        .expect("insert");
    assert!(fail_id > ok_id);

    let conn = rusqlite::Connection::open(&path).expect("raw open");
    let mut stmt = conn
        .prepare(
            "SELECT ts, ok, status_code, latency_ms, error_kind, error_detail
                 FROM checks WHERE target_id = 'opac' ORDER BY ts",
        )
        .expect("prepare");
    let rows: Vec<CheckRow> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], (1000, 1, Some(200), Some(123), None, None));
    assert_eq!(
        rows[1],
        (
            1300,
            0,
            None,
            Some(10_000),
            Some("timeout".to_string()),
            Some("operation timed out".to_string())
        )
    );
}

#[tokio::test]
async fn chart_since_returns_every_check_with_its_latency() {
    let (_dir, _path, store) = open_temp().await;
    store
        .upsert_targets(&[target("opac", "OPAC")])
        .await
        .expect("upsert");
    store
        .insert_check("opac", 1000, &success())
        .await
        .expect("insert");
    store
        .insert_check("opac", 1300, &failure())
        .await
        .expect("insert");
    store
        .insert_check("opac", 1600, &success())
        .await
        .expect("insert");

    // A failed check keeps the latency the probe measured; the chart
    // treats it as an outage, not as a sample.
    assert_eq!(
        store.chart_since("opac", 1300).await.expect("chart"),
        vec![(1300, false, Some(10_000)), (1600, true, Some(123))]
    );
}

#[tokio::test]
async fn insert_check_rejects_unknown_target() {
    let (_dir, _path, store) = open_temp().await;
    let err = store
        .insert_check("nope", unix_now(), &success())
        .await
        .expect_err("foreign key must reject");
    assert!(
        err.to_string().to_lowercase().contains("foreign key"),
        "{err}"
    );
}

#[tokio::test]
async fn target_history_is_empty_for_an_unchecked_target() {
    let (_dir, _path, store) = open_temp().await;
    store
        .upsert_targets(&[target("opac", "OPAC")])
        .await
        .expect("upsert");
    let history = store.target_history("opac").await.expect("history");
    assert_eq!(
        history,
        TargetHistory {
            open_incident: None,
            run_head: None,
            run_len: 0,
        }
    );
}

#[tokio::test]
async fn target_history_reports_the_trailing_run_and_its_first_check() {
    let (_dir, _path, store) = open_temp().await;
    store
        .upsert_targets(&[target("opac", "OPAC")])
        .await
        .expect("upsert");
    store
        .insert_check("opac", 900, &success())
        .await
        .expect("insert");
    store
        .insert_check("opac", 1000, &failure())
        .await
        .expect("insert");
    store
        .insert_check("opac", 1100, &failure())
        .await
        .expect("insert");

    let history = store.target_history("opac").await.expect("history");
    assert_eq!(history.open_incident, None);
    assert_eq!(history.run_len, 2);
    let head = history.run_head.expect("head");
    assert!(!head.ok);
    assert_eq!(head.ts, 1000);
    assert_eq!(head.error_kind.as_deref(), Some("timeout"));
    assert_eq!(head.error_detail.as_deref(), Some("operation timed out"));

    store
        .insert_check("opac", 1200, &success())
        .await
        .expect("insert");
    let history = store.target_history("opac").await.expect("history");
    assert_eq!(history.run_len, 1);
    let head = history.run_head.expect("head");
    assert!(head.ok);
    assert_eq!(head.ts, 1200);
    assert_eq!(head.error_kind, None);
}

#[tokio::test]
async fn target_history_reports_the_open_incident() {
    let (_dir, _path, store) = open_temp().await;
    store
        .upsert_targets(&[target("opac", "OPAC")])
        .await
        .expect("upsert");
    let id = store
        .open_incident("opac", 1000, "timeout", Some("operation timed out".into()))
        .await
        .expect("open");
    assert_eq!(
        store
            .target_history("opac")
            .await
            .expect("history")
            .open_incident,
        Some(id)
    );
    store.close_incident(id, 1200).await.expect("close");
    assert_eq!(
        store
            .target_history("opac")
            .await
            .expect("history")
            .open_incident,
        None
    );
}

#[tokio::test]
async fn only_one_incident_can_be_open_per_target() {
    let (_dir, _path, store) = open_temp().await;
    store
        .upsert_targets(&[target("opac", "OPAC")])
        .await
        .expect("upsert");
    let first = store
        .open_incident("opac", 1000, "timeout", None)
        .await
        .expect("open");
    let err = store
        .open_incident("opac", 1100, "timeout", None)
        .await
        .expect_err("second open must be rejected");
    assert!(err.to_string().to_lowercase().contains("unique"), "{err}");

    store.close_incident(first, 1200).await.expect("close");
    store
        .open_incident("opac", 1300, "timeout", None)
        .await
        .expect("open after close");
}

#[tokio::test]
async fn closing_an_already_closed_incident_is_an_error() {
    let (_dir, _path, store) = open_temp().await;
    store
        .upsert_targets(&[target("opac", "OPAC")])
        .await
        .expect("upsert");
    let id = store
        .open_incident("opac", 1000, "timeout", None)
        .await
        .expect("open");
    store.close_incident(id, 1200).await.expect("close");
    let err = store
        .close_incident(id, 1300)
        .await
        .expect_err("second close must be rejected");
    assert!(err.to_string().contains("not open"), "{err}");
}

#[tokio::test]
async fn feed_incidents_filters_scope_and_window() {
    let (_dir, _path, store) = open_temp().await;
    let mut disabled = target("b", "B");
    disabled.enabled = false;
    store
        .upsert_targets(&[target("a", "A"), disabled])
        .await
        .expect("upsert");

    let old = store
        .open_incident("a", 100, "timeout", None)
        .await
        .expect("open old");
    store.close_incident(old, 200).await.expect("close old");
    let recent = store
        .open_incident("a", 300, "status", Some("HTTP 503, erwartet 200".into()))
        .await
        .expect("open recent");
    store
        .close_incident(recent, 400)
        .await
        .expect("close recent");
    store
        .open_incident("a", 500, "dns", None)
        .await
        .expect("open running");
    let disabled_incident = store
        .open_incident("b", 600, "connect", None)
        .await
        .expect("open disabled");
    store
        .close_incident(disabled_incident, 700)
        .await
        .expect("close disabled");

    let all = store
        .feed_incidents(None, 300, true)
        .await
        .expect("all feed");
    assert_eq!(
        all.iter()
            .map(|incident| (incident.started_at, incident.ended_at))
            .collect::<Vec<_>>(),
        vec![(300, Some(400)), (500, None)]
    );
    assert!(all.iter().all(|incident| incident.target_id == "a"));

    // An old incident that is still open survives the window.
    let since_newer = store
        .feed_incidents(Some("a"), 600, false)
        .await
        .expect("open incident");
    assert_eq!(since_newer.len(), 1);
    assert_eq!(since_newer[0].ended_at, None);

    // A disabled target keeps its own feed readable.
    let one = store
        .feed_incidents(Some("b"), 300, false)
        .await
        .expect("target feed");
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].target_name, "B");
    assert_eq!(one[0].cause, "connect");
}

#[test]
fn unix_now_is_plausible() {
    let now = unix_now();
    assert!(now > 1_700_000_000, "{now}");
}
