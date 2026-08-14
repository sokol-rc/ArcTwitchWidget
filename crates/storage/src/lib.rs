use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use arc_live_core::redaction::sanitize_json;
use arc_live_core::state::OverlayStats;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone)]
pub struct Storage {
    path: PathBuf,
    connection: Arc<Mutex<Connection>>,
    observation_writes: Arc<AtomicU64>,
    user_event_writes: Arc<AtomicU64>,
}

const MAX_OBSERVATIONS: usize = 5_000;
const MAX_USER_EVENTS: usize = 1_000;
pub const STREAM_SESSION_SCHEMA_VERSION: u8 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub id: i64,
    pub observed_at: DateTime<Utc>,
    pub direction: String,
    pub host: String,
    pub method: Option<String>,
    pub path: Option<String>,
    pub status: Option<u16>,
    pub content_type: Option<String>,
    pub shape: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedStreamSession {
    pub schema_version: u8,
    pub local_day: String,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub baseline: OverlayStats,
    pub overlay: OverlayStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserEvent {
    pub at: DateTime<Utc>,
    pub local_day: String,
    pub level: String,
    pub message: String,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)
            .with_context(|| format!("opening SQLite database {}", path.display()))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.pragma_update(None, "journal_size_limit", 8 * 1024 * 1024_i64)?;
        connection.pragma_update(None, "wal_autocheckpoint", 512_i64)?;
        connection.pragma_update(None, "cache_size", -2_048_i64)?;
        connection.pragma_update(None, "temp_store", "MEMORY")?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS observations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                observed_at TEXT NOT NULL,
                direction TEXT NOT NULL,
                host TEXT NOT NULL,
                method TEXT,
                path TEXT,
                status INTEGER,
                content_type TEXT,
                shape_json TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS observations_time_idx
              ON observations(observed_at DESC);
            CREATE TABLE IF NOT EXISTS stream_session (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                schema_version INTEGER NOT NULL DEFAULT 1,
                local_day TEXT NOT NULL,
                started_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                baseline_json TEXT NOT NULL,
                overlay_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS user_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                at TEXT NOT NULL,
                local_day TEXT NOT NULL,
                level TEXT NOT NULL,
                message TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS user_events_day_idx
              ON user_events(local_day, id DESC);
            INSERT OR IGNORE INTO schema_migrations(version, applied_at)
              VALUES (1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
            INSERT OR IGNORE INTO schema_migrations(version, applied_at)
              VALUES (2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
            "#,
        )?;
        if !Self::table_has_column(&connection, "stream_session", "schema_version")? {
            connection.execute(
                "ALTER TABLE stream_session ADD COLUMN schema_version INTEGER NOT NULL DEFAULT 1",
                [],
            )?;
        }
        connection.execute(
            "INSERT OR IGNORE INTO schema_migrations(version, applied_at) VALUES (3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
            [],
        )?;
        Self::prune_history(&connection)?;
        connection.execute_batch("PRAGMA optimize;")?;
        Ok(Self {
            path: path.to_path_buf(),
            connection: Arc::new(Mutex::new(connection)),
            observation_writes: Arc::new(AtomicU64::new(0)),
            user_event_writes: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn insert_observation(&self, observation: &Observation) -> Result<i64> {
        let sanitized = sanitize_json(&observation.shape);
        let connection = self.connection.lock().expect("storage mutex poisoned");
        connection.execute(
            "INSERT INTO observations(observed_at, direction, host, method, path, status, content_type, shape_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                observation.observed_at.to_rfc3339(),
                observation.direction,
                observation.host,
                observation.method,
                observation.path,
                observation.status.map(i64::from),
                observation.content_type,
                serde_json::to_string(&sanitized)?,
            ],
        )?;
        if self.observation_writes.fetch_add(1, Ordering::Relaxed) % 256 == 255 {
            connection.execute(
                "DELETE FROM observations WHERE id NOT IN (SELECT id FROM observations ORDER BY id DESC LIMIT ?1)",
                [MAX_OBSERVATIONS as i64],
            )?;
        }
        Ok(connection.last_insert_rowid())
    }

    pub fn recent_observations(&self, limit: usize) -> Result<Vec<Observation>> {
        let connection = self.connection.lock().expect("storage mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT id, observed_at, direction, host, method, path, status, content_type, shape_json FROM observations ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit as i64], |row| {
            let observed_at: String = row.get(1)?;
            let shape: String = row.get(8)?;
            Ok(Observation {
                id: row.get(0)?,
                observed_at: DateTime::parse_from_rfc3339(&observed_at)
                    .map(|value| value.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                direction: row.get(2)?,
                host: row.get(3)?,
                method: row.get(4)?,
                path: row.get(5)?,
                status: row.get::<_, Option<i64>>(6)?.map(|value| value as u16),
                content_type: row.get(7)?,
                shape: serde_json::from_str(&shape).unwrap_or(Value::Null),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn save_stream_session(&self, session: &PersistedStreamSession) -> Result<()> {
        let connection = self.connection.lock().expect("storage mutex poisoned");
        connection.execute(
            r#"
            INSERT INTO stream_session(id, schema_version, local_day, started_at, updated_at, baseline_json, overlay_json)
            VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(id) DO UPDATE SET
              schema_version = excluded.schema_version,
              local_day = excluded.local_day,
              started_at = excluded.started_at,
              updated_at = excluded.updated_at,
              baseline_json = excluded.baseline_json,
              overlay_json = excluded.overlay_json
            "#,
            params![
                i64::from(session.schema_version),
                session.local_day,
                session.started_at.to_rfc3339(),
                session.updated_at.to_rfc3339(),
                serde_json::to_string(&session.baseline)?,
                serde_json::to_string(&session.overlay)?,
            ],
        )?;
        Ok(())
    }

    pub fn load_stream_session(&self) -> Result<Option<PersistedStreamSession>> {
        let connection = self.connection.lock().expect("storage mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT schema_version, local_day, started_at, updated_at, baseline_json, overlay_json FROM stream_session WHERE id = 1",
        )?;
        let mut rows = statement.query([])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let schema_version = row.get::<_, i64>(0)? as u8;
        if schema_version != STREAM_SESSION_SCHEMA_VERSION {
            return Ok(None);
        }
        let started_at: String = row.get(2)?;
        let updated_at: String = row.get(3)?;
        let baseline_json: String = row.get(4)?;
        let overlay_json: String = row.get(5)?;
        Ok(Some(PersistedStreamSession {
            schema_version,
            local_day: row.get(1)?,
            started_at: DateTime::parse_from_rfc3339(&started_at)
                .context("parsing persisted stream start")?
                .with_timezone(&Utc),
            updated_at: DateTime::parse_from_rfc3339(&updated_at)
                .context("parsing persisted stream update")?
                .with_timezone(&Utc),
            baseline: serde_json::from_str(&baseline_json)
                .context("parsing persisted stream baseline")?,
            overlay: serde_json::from_str(&overlay_json)
                .context("parsing persisted stream overlay")?,
        }))
    }

    pub fn insert_user_event(&self, event: &UserEvent) -> Result<i64> {
        let connection = self.connection.lock().expect("storage mutex poisoned");
        connection.execute(
            "INSERT INTO user_events(at, local_day, level, message) VALUES (?1, ?2, ?3, ?4)",
            params![
                event.at.to_rfc3339(),
                event.local_day,
                event.level,
                event.message,
            ],
        )?;
        if self.user_event_writes.fetch_add(1, Ordering::Relaxed) % 64 == 63 {
            connection.execute(
                "DELETE FROM user_events WHERE id NOT IN (SELECT id FROM user_events ORDER BY id DESC LIMIT ?1)",
                [MAX_USER_EVENTS as i64],
            )?;
        }
        Ok(connection.last_insert_rowid())
    }

    pub fn user_events_for_day(&self, local_day: &str, limit: usize) -> Result<Vec<UserEvent>> {
        let connection = self.connection.lock().expect("storage mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT at, local_day, level, message FROM user_events WHERE local_day = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![local_day, limit.min(100) as i64], |row| {
            let at: String = row.get(0)?;
            Ok(UserEvent {
                at: DateTime::parse_from_rfc3339(&at)
                    .map(|value| value.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                local_day: row.get(1)?,
                level: row.get(2)?,
                message: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn prune_history(connection: &Connection) -> Result<()> {
        connection.execute(
            "DELETE FROM observations WHERE id NOT IN (SELECT id FROM observations ORDER BY id DESC LIMIT ?1)",
            [MAX_OBSERVATIONS as i64],
        )?;
        connection.execute(
            "DELETE FROM user_events WHERE id NOT IN (SELECT id FROM user_events ORDER BY id DESC LIMIT ?1)",
            [MAX_USER_EVENTS as i64],
        )?;
        Ok(())
    }

    fn table_has_column(connection: &Connection, table: &str, column: &str) -> Result<bool> {
        let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
        let names = statement.query_map([], |row| row.get::<_, String>(1))?;
        for name in names {
            if name? == column {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn persists_only_sanitized_observations() {
        let storage = Storage::open(Path::new(":memory:")).unwrap();
        storage
            .insert_observation(&Observation {
                id: 0,
                observed_at: Utc::now(),
                direction: "response".into(),
                host: "api.example.test".into(),
                method: Some("GET".into()),
                path: Some("/rounds".into()),
                status: Some(200),
                content_type: Some("application/json".into()),
                shape: json!({"accessToken": "must-not-survive", "rounds": [{"id": "x"}]}),
            })
            .unwrap();
        let rows = storage.recent_observations(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].shape["accessToken"], "[REDACTED]");
        assert_eq!(rows[0].shape["rounds"][0]["id"], "x");
    }

    #[test]
    fn persists_stream_session_and_today_events() {
        let storage = Storage::open(Path::new(":memory:")).unwrap();
        let now = Utc::now();
        let baseline = OverlayStats {
            downs: 10,
            ..Default::default()
        };
        let overlay = OverlayStats {
            downs: 14,
            session_downs: 4,
            ..Default::default()
        };
        storage
            .save_stream_session(&PersistedStreamSession {
                schema_version: STREAM_SESSION_SCHEMA_VERSION,
                local_day: "2026-08-14".into(),
                started_at: now,
                updated_at: now,
                baseline,
                overlay,
            })
            .unwrap();
        storage
            .insert_user_event(&UserEvent {
                at: now,
                local_day: "2026-08-14".into(),
                level: "success".into(),
                message: "Статистика восстановлена".into(),
            })
            .unwrap();

        let restored = storage.load_stream_session().unwrap().unwrap();
        assert_eq!(restored.overlay.session_downs, 4);
        let events = storage.user_events_for_day("2026-08-14", 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].message, "Статистика восстановлена");
    }

    #[test]
    fn restores_stream_counters_after_database_reopen() {
        let unique = format!(
            "arc-live-recovery-{}-{}.sqlite3",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let path = std::env::temp_dir().join(unique);
        let now = Utc::now();
        let baseline = OverlayStats {
            downs: 10,
            extractions: 20,
            deaths: 30,
            loot_value: 40_000,
            value_brought_in: 8_000,
            ..Default::default()
        };
        let mut before_restart = OverlayStats {
            downs: 14,
            extractions: 21,
            deaths: 31,
            loot_value: 50_000,
            value_brought_in: 12_000,
            ..Default::default()
        };
        before_restart.apply_session_baseline(&baseline);

        {
            let storage = Storage::open(&path).unwrap();
            storage
                .save_stream_session(&PersistedStreamSession {
                    schema_version: STREAM_SESSION_SCHEMA_VERSION,
                    local_day: "2026-08-14".into(),
                    started_at: now,
                    updated_at: now,
                    baseline,
                    overlay: before_restart,
                })
                .unwrap();
        }

        {
            let storage = Storage::open(&path).unwrap();
            let restored = storage.load_stream_session().unwrap().unwrap();
            assert_eq!(restored.overlay.session_downs, 4);

            let mut after_restart = OverlayStats {
                downs: 16,
                extractions: 23,
                deaths: 32,
                loot_value: 60_000,
                value_brought_in: 15_000,
                ..Default::default()
            };
            after_restart.apply_session_baseline(&restored.baseline);
            assert_eq!(after_restart.session_downs, 6);
            assert_eq!(after_restart.session_extractions, 3);
            assert_eq!(after_restart.session_deaths, 2);
            assert_eq!(after_restart.session_money_delta, 13_000);
        }

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[test]
    fn maintenance_caps_long_running_history() {
        let storage = Storage::open(Path::new(":memory:")).unwrap();
        let connection = storage.connection.lock().expect("storage mutex poisoned");
        connection
            .execute_batch(
                r#"
                WITH RECURSIVE rows(value) AS (
                  SELECT 1 UNION ALL SELECT value + 1 FROM rows WHERE value < 5200
                )
                INSERT INTO observations(observed_at, direction, host, shape_json)
                  SELECT '2026-08-14T00:00:00Z', 'response', 'example.test', '{}' FROM rows;
                WITH RECURSIVE rows(value) AS (
                  SELECT 1 UNION ALL SELECT value + 1 FROM rows WHERE value < 1200
                )
                INSERT INTO user_events(at, local_day, level, message)
                  SELECT '2026-08-14T00:00:00Z', '2026-08-14', 'info', 'event' FROM rows;
                "#,
            )
            .unwrap();
        Storage::prune_history(&connection).unwrap();
        let observations: i64 = connection
            .query_row("SELECT COUNT(*) FROM observations", [], |row| row.get(0))
            .unwrap();
        let events: i64 = connection
            .query_row("SELECT COUNT(*) FROM user_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(observations, MAX_OBSERVATIONS as i64);
        assert_eq!(events, MAX_USER_EVENTS as i64);
    }

    #[test]
    fn legacy_session_snapshot_is_not_restored_as_live_data() {
        let storage = Storage::open(Path::new(":memory:")).unwrap();
        let now = Utc::now().to_rfc3339();
        let legacy = serde_json::to_string(&OverlayStats {
            downs: 12,
            raider_damage: 34,
            loot_value: 56,
            ..Default::default()
        })
        .unwrap();
        storage
            .connection
            .lock()
            .expect("storage mutex poisoned")
            .execute(
                "INSERT INTO stream_session(id, local_day, started_at, updated_at, baseline_json, overlay_json) VALUES (1, '2026-08-14', ?1, ?1, ?2, ?2)",
                params![now, legacy],
            )
            .unwrap();

        assert!(storage.load_stream_session().unwrap().is_none());
    }
}
