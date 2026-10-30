//! SQLite storage: schema migrations, target sync, check inserts.
//!
//! One writer connection shared through `tokio-rusqlite`; every operation runs
//! on that connection's dedicated thread, so no lock handling leaks into the
//! async code.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use tokio_rusqlite::Connection;

use crate::config::TargetConfig;
use crate::probe::Outcome;

/// Ordered schema migrations; step `n` moves `PRAGMA user_version` to `n + 1`.
const MIGRATIONS: &[&str] = &[SCHEMA_V1];

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
mod tests {
    use super::*;
    use crate::probe::ErrorKind;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// `checks` row as read back by the tests: ts, ok, status, latency, kind, detail.
    type CheckRow = (i64, i64, Option<i64>, Option<i64>, Option<String>, Option<String>);

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
        store.upsert_targets(&[target("opac", "OPAC")]).await.expect("upsert");
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

        let ok_id = store.insert_check("opac", 1000, &success()).await.expect("insert");
        let fail_id = store.insert_check("opac", 1300, &failure()).await.expect("insert");
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
    async fn insert_check_rejects_unknown_target() {
        let (_dir, _path, store) = open_temp().await;
        let err = store
            .insert_check("nope", unix_now(), &success())
            .await
            .expect_err("foreign key must reject");
        assert!(err.to_string().to_lowercase().contains("foreign key"), "{err}");
    }

    #[test]
    fn unix_now_is_plausible() {
        let now = unix_now();
        assert!(now > 1_700_000_000, "{now}");
    }
}
