//! Conductor-owned control store. This is not a guild or core application database.
use anyhow::{Context as _, Result};
use serde::Serialize;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rand::Rng;

use crate::settings::{ControlSettings, SETTINGS_KEY};

const SCHEMA_VERSION: &str = "1";

pub struct ControlStore {
    db: libsql::Database,
}

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub operator_id: String,
    pub created_ms: i64,
    pub last_seen_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobRow {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub payload_json: String,
    pub attempts: i64,
    pub max_attempts: i64,
    pub due_ms: i64,
    pub last_error: Option<String>,
    pub created_ms: i64,
    pub finished_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TelemetryRow {
    pub id: i64,
    pub collected_ms: i64,
    pub kind: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct NotifyReceipt {
    pub id: i64,
    pub sent_ms: i64,
    pub category: String,
    pub outcome: String,
    pub detail: Option<String>,
}

impl ControlStore {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .context("failed to create conductor control database directory")?;
        }
        let db = libsql::Builder::new_local(path)
            .build()
            .await
            .context("failed to open conductor control database")?;
        let store = Self { db };
        store.migrate().await?;
        Ok(store)
    }

    async fn connect(&self) -> Result<libsql::Connection> {
        self.db
            .connect()
            .context("failed to connect to conductor control database")
    }

    async fn migrate(&self) -> Result<()> {
        let conn = self.connect().await?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS meta (
              key TEXT PRIMARY KEY,
              value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS sessions (
              id TEXT PRIMARY KEY,
              operator_id TEXT NOT NULL,
              created_ms INTEGER NOT NULL,
              last_seen_ms INTEGER NOT NULL,
              revoked INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS settings (
              key TEXT PRIMARY KEY,
              value_json TEXT NOT NULL,
              applied INTEGER NOT NULL,
              revision INTEGER NOT NULL,
              updated_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS jobs (
              id TEXT PRIMARY KEY,
              kind TEXT NOT NULL,
              status TEXT NOT NULL,
              payload_json TEXT NOT NULL,
              attempts INTEGER NOT NULL,
              max_attempts INTEGER NOT NULL,
              due_ms INTEGER NOT NULL,
              last_error TEXT,
              created_ms INTEGER NOT NULL,
              finished_ms INTEGER
            );
            CREATE TABLE IF NOT EXISTS telemetry (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              collected_ms INTEGER NOT NULL,
              kind TEXT NOT NULL,
              payload_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS notify_receipts (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              sent_ms INTEGER NOT NULL,
              category TEXT NOT NULL,
              outcome TEXT NOT NULL,
              detail TEXT
            );
            CREATE TABLE IF NOT EXISTS leases (
              session_id TEXT PRIMARY KEY,
              until_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS oauth_states (
              state TEXT PRIMARY KEY,
              created_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS audit (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              at_ms INTEGER NOT NULL,
              actor TEXT NOT NULL,
              action TEXT NOT NULL,
              detail TEXT
            );
            CREATE INDEX IF NOT EXISTS telemetry_collected ON telemetry(collected_ms);
            CREATE INDEX IF NOT EXISTS jobs_due ON jobs(status, due_ms);
            "#,
        )
        .await
        .context("failed to initialize conductor control schema")?;
        conn.execute(
            "INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', ?)",
            libsql::params![SCHEMA_VERSION],
        )
        .await?;
        let mut rows = conn
            .query(
                "SELECT 1 FROM settings WHERE key = ?",
                libsql::params![SETTINGS_KEY],
            )
            .await?;
        if rows.next().await?.is_none() {
            let now = now_ms();
            let defaults = serde_json::to_string(&ControlSettings::default())?;
            conn.execute(
                "INSERT INTO settings(key, value_json, applied, revision, updated_ms) VALUES (?, ?, 1, 1, ?)",
                libsql::params![SETTINGS_KEY, defaults, now],
            )
            .await?;
        }
        Ok(())
    }

    pub async fn meta_get(&self, key: &str) -> Result<Option<String>> {
        let conn = self.connect().await?;
        Self::meta_get_conn(&conn, key).await
    }

    async fn meta_get_conn(conn: &libsql::Connection, key: &str) -> Result<Option<String>> {
        let mut rows = conn
            .query("SELECT value FROM meta WHERE key = ?", libsql::params![key])
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some(row.get::<String>(0)?)),
            None => Ok(None),
        }
    }

    pub async fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.connect().await?;
        conn.execute(
            "INSERT INTO meta(key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            libsql::params![key, value],
        )
        .await?;
        Ok(())
    }

    pub async fn setup_operator_id(&self) -> Result<Option<String>> {
        self.meta_get("setup_operator_id").await
    }

    pub async fn stamp_setup_identity(&self, operator_id: &str) -> Result<()> {
        let conn = self.connect().await?;
        conn.execute("BEGIN IMMEDIATE", ()).await?;
        let current = Self::meta_get_conn(&conn, "setup_operator_id").await?;
        if let Some(existing) = current {
            conn.execute("ROLLBACK", ()).await.ok();
            anyhow::ensure!(
                existing == operator_id,
                "conductor setup identity is already bound to a different operator"
            );
            return Ok(());
        }
        conn.execute(
            "INSERT INTO meta(key, value) VALUES ('setup_operator_id', ?)",
            libsql::params![operator_id],
        )
        .await?;
        conn.execute(
            "INSERT INTO meta(key, value) VALUES ('setup_complete', '1') ON CONFLICT(key) DO UPDATE SET value = '1'",
            (),
        )
        .await?;
        conn.execute("COMMIT", ()).await?;
        Ok(())
    }

    pub async fn load_settings(&self) -> Result<(ControlSettings, i64, i64)> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT value_json, revision, updated_ms FROM settings WHERE key = ?",
                libsql::params![SETTINGS_KEY],
            )
            .await?;
        let row = rows.next().await?.context("missing control settings")?;
        let json: String = row.get(0)?;
        let revision: i64 = row.get(1)?;
        let updated_ms: i64 = row.get(2)?;
        let settings: ControlSettings = serde_json::from_str(&json)?;
        Ok((settings, revision, updated_ms))
    }

    pub async fn save_settings(&self, settings: &ControlSettings, expected: i64) -> Result<i64> {
        let conn = self.connect().await?;
        let json = serde_json::to_string(settings)?;
        let now = now_ms();
        let changed = conn
            .execute(
                "UPDATE settings SET value_json = ?, applied = 1, revision = revision + 1, updated_ms = ? \
                 WHERE key = ? AND revision = ?",
                libsql::params![json, now, SETTINGS_KEY, expected],
            )
            .await?;
        anyhow::ensure!(changed == 1, "control settings were updated concurrently");
        Ok(expected + 1)
    }

    pub async fn put_oauth_state(&self, state: &str) -> Result<()> {
        let conn = self.connect().await?;
        conn.execute(
            "INSERT INTO oauth_states(state, created_ms) VALUES (?, ?)",
            libsql::params![state, now_ms()],
        )
        .await?;
        conn.execute(
            "DELETE FROM oauth_states WHERE created_ms < ?",
            libsql::params![now_ms() - 10 * 60 * 1000],
        )
        .await?;
        Ok(())
    }

    pub async fn take_oauth_state(&self, state: &str) -> Result<bool> {
        let conn = self.connect().await?;
        let changed = conn
            .execute(
                "DELETE FROM oauth_states WHERE state = ? AND created_ms >= ?",
                libsql::params![state, now_ms() - 10 * 60 * 1000],
            )
            .await?;
        Ok(changed == 1)
    }

    pub async fn create_session(&self, operator_id: &str) -> Result<String> {
        let id = random_token();
        let now = now_ms();
        let conn = self.connect().await?;
        conn.execute(
            "INSERT INTO sessions(id, operator_id, created_ms, last_seen_ms, revoked) VALUES (?, ?, ?, ?, 0)",
            libsql::params![id.clone(), operator_id, now, now],
        )
        .await?;
        Ok(id)
    }

    pub async fn load_session(&self, id: &str) -> Result<Option<SessionRow>> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT id, operator_id, created_ms, last_seen_ms FROM sessions WHERE id = ? AND revoked = 0",
                libsql::params![id],
            )
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some(SessionRow {
                id: row.get(0)?,
                operator_id: row.get(1)?,
                created_ms: row.get(2)?,
                last_seen_ms: row.get(3)?,
            })),
            None => Ok(None),
        }
    }

    pub async fn touch_session(&self, id: &str) -> Result<()> {
        let conn = self.connect().await?;
        conn.execute(
            "UPDATE sessions SET last_seen_ms = ? WHERE id = ? AND revoked = 0",
            libsql::params![now_ms(), id],
        )
        .await?;
        Ok(())
    }

    pub async fn revoke_session(&self, id: &str) -> Result<()> {
        let conn = self.connect().await?;
        conn.execute(
            "UPDATE sessions SET revoked = 1 WHERE id = ?",
            libsql::params![id],
        )
        .await?;
        conn.execute(
            "DELETE FROM leases WHERE session_id = ?",
            libsql::params![id],
        )
        .await?;
        Ok(())
    }

    pub async fn put_lease(&self, session_id: &str, until_ms: i64) -> Result<()> {
        let conn = self.connect().await?;
        conn.execute(
            "INSERT INTO leases(session_id, until_ms) VALUES (?, ?) \
             ON CONFLICT(session_id) DO UPDATE SET until_ms = excluded.until_ms",
            libsql::params![session_id, until_ms],
        )
        .await?;
        Ok(())
    }

    pub async fn has_live_lease(&self) -> Result<bool> {
        let conn = self.connect().await?;
        let now = now_ms();
        conn.execute(
            "DELETE FROM leases WHERE until_ms < ?",
            libsql::params![now],
        )
        .await?;
        let mut rows = conn
            .query(
                "SELECT 1 FROM leases WHERE until_ms >= ? LIMIT 1",
                libsql::params![now],
            )
            .await?;
        Ok(rows.next().await?.is_some())
    }

    pub async fn insert_job(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        max_attempts: i64,
    ) -> Result<String> {
        let id = random_token();
        let now = now_ms();
        let conn = self.connect().await?;
        conn.execute(
            "INSERT INTO jobs(id, kind, status, payload_json, attempts, max_attempts, due_ms, created_ms) \
             VALUES (?, ?, 'queued', ?, 0, ?, ?, ?)",
            libsql::params![id.clone(), kind, payload.to_string(), max_attempts, now, now],
        )
        .await?;
        Ok(id)
    }

    pub async fn claim_job(&self) -> Result<Option<JobRow>> {
        let conn = self.connect().await?;
        let now = now_ms();
        conn.execute("BEGIN IMMEDIATE", ()).await?;
        let mut rows = conn
            .query(
                "SELECT id FROM jobs WHERE status = 'queued' AND due_ms <= ? ORDER BY due_ms LIMIT 1",
                libsql::params![now],
            )
            .await?;
        let Some(row) = rows.next().await? else {
            conn.execute("ROLLBACK", ()).await.ok();
            return Ok(None);
        };
        let id: String = row.get(0)?;
        conn.execute(
            "UPDATE jobs SET status = 'running', attempts = attempts + 1 WHERE id = ? AND status = 'queued'",
            libsql::params![id.clone()],
        )
        .await?;
        let job = Self::job_conn(&conn, &id).await?;
        conn.execute("COMMIT", ()).await?;
        Ok(job)
    }

    async fn job_conn(conn: &libsql::Connection, id: &str) -> Result<Option<JobRow>> {
        let mut rows = conn
            .query(
                "SELECT id, kind, status, payload_json, attempts, max_attempts, due_ms, last_error, created_ms, finished_ms \
                 FROM jobs WHERE id = ?",
                libsql::params![id],
            )
            .await?;
        Ok(rows.next().await?.map(|row| JobRow {
            id: row.get(0).unwrap_or_default(),
            kind: row.get(1).unwrap_or_default(),
            status: row.get(2).unwrap_or_default(),
            payload_json: row.get(3).unwrap_or_default(),
            attempts: row.get(4).unwrap_or(0),
            max_attempts: row.get(5).unwrap_or(0),
            due_ms: row.get(6).unwrap_or(0),
            last_error: row.get(7).ok(),
            created_ms: row.get(8).unwrap_or(0),
            finished_ms: row.get(9).ok(),
        }))
    }

    pub async fn finish_job(&self, id: &str, ok: bool, error: Option<&str>) -> Result<()> {
        let conn = self.connect().await?;
        let now = now_ms();
        if ok {
            conn.execute(
                "UPDATE jobs SET status = 'succeeded', last_error = NULL, finished_ms = ? WHERE id = ?",
                libsql::params![now, id],
            )
            .await?;
            return Ok(());
        }
        let mut rows = conn
            .query(
                "SELECT attempts, max_attempts FROM jobs WHERE id = ?",
                libsql::params![id],
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(());
        };
        let attempts: i64 = row.get(0)?;
        let max_attempts: i64 = row.get(1)?;
        if attempts < max_attempts {
            let delay = (1_i64 << attempts.min(8)) * 1000;
            conn.execute(
                "UPDATE jobs SET status = 'queued', due_ms = ?, last_error = ? WHERE id = ?",
                libsql::params![now + delay, error.unwrap_or("failed"), id],
            )
            .await?;
        } else {
            conn.execute(
                "UPDATE jobs SET status = 'failed', last_error = ?, finished_ms = ? WHERE id = ?",
                libsql::params![error.unwrap_or("failed"), now, id],
            )
            .await?;
        }
        Ok(())
    }

    pub async fn replay_job(&self, id: &str) -> Result<bool> {
        let conn = self.connect().await?;
        let changed = conn
            .execute(
                "UPDATE jobs SET status = 'queued', due_ms = ?, last_error = NULL, finished_ms = NULL \
                 WHERE id = ? AND status = 'failed'",
                libsql::params![now_ms(), id],
            )
            .await?;
        Ok(changed == 1)
    }

    pub async fn list_jobs(&self, limit: i64) -> Result<Vec<JobRow>> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT id, kind, status, payload_json, attempts, max_attempts, due_ms, last_error, created_ms, finished_ms \
                 FROM jobs ORDER BY created_ms DESC LIMIT ?",
                libsql::params![limit],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(JobRow {
                id: row.get(0)?,
                kind: row.get(1)?,
                status: row.get(2)?,
                payload_json: row.get(3)?,
                attempts: row.get(4)?,
                max_attempts: row.get(5)?,
                due_ms: row.get(6)?,
                last_error: row.get(7).ok(),
                created_ms: row.get(8)?,
                finished_ms: row.get(9).ok(),
            });
        }
        Ok(out)
    }

    pub async fn insert_telemetry(&self, kind: &str, payload: &serde_json::Value) -> Result<()> {
        let conn = self.connect().await?;
        conn.execute(
            "INSERT INTO telemetry(collected_ms, kind, payload_json) VALUES (?, ?, ?)",
            libsql::params![now_ms(), kind, payload.to_string()],
        )
        .await?;
        Ok(())
    }

    pub async fn prune_telemetry(&self, oldest_ms: i64, max_rows: i64) -> Result<()> {
        let conn = self.connect().await?;
        conn.execute(
            "DELETE FROM telemetry WHERE collected_ms < ?",
            libsql::params![oldest_ms],
        )
        .await?;
        conn.execute(
            "DELETE FROM telemetry WHERE id NOT IN (SELECT id FROM telemetry ORDER BY collected_ms DESC LIMIT ?)",
            libsql::params![max_rows],
        )
        .await?;
        Ok(())
    }

    pub async fn telemetry_range(
        &self,
        from_ms: i64,
        to_ms: i64,
        limit: i64,
    ) -> Result<Vec<TelemetryRow>> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT id, collected_ms, kind, payload_json FROM telemetry \
                 WHERE collected_ms >= ? AND collected_ms <= ? ORDER BY collected_ms DESC LIMIT ?",
                libsql::params![from_ms, to_ms, limit],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let json: String = row.get(3)?;
            out.push(TelemetryRow {
                id: row.get(0)?,
                collected_ms: row.get(1)?,
                kind: row.get(2)?,
                payload: serde_json::from_str(&json).unwrap_or(serde_json::Value::Null),
            });
        }
        Ok(out)
    }

    pub async fn insert_notify_receipt(
        &self,
        category: &str,
        outcome: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        let conn = self.connect().await?;
        conn.execute(
            "INSERT INTO notify_receipts(sent_ms, category, outcome, detail) VALUES (?, ?, ?, ?)",
            libsql::params![now_ms(), category, outcome, detail.unwrap_or("")],
        )
        .await?;
        conn.execute(
            "DELETE FROM notify_receipts WHERE id NOT IN (SELECT id FROM notify_receipts ORDER BY sent_ms DESC LIMIT 200)",
            (),
        )
        .await?;
        Ok(())
    }

    pub async fn recent_notify_receipts(&self, limit: i64) -> Result<Vec<NotifyReceipt>> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT id, sent_ms, category, outcome, detail FROM notify_receipts ORDER BY sent_ms DESC LIMIT ?",
                libsql::params![limit],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(NotifyReceipt {
                id: row.get(0)?,
                sent_ms: row.get(1)?,
                category: row.get(2)?,
                outcome: row.get(3)?,
                detail: row.get::<String>(4).ok().filter(|s| !s.is_empty()),
            });
        }
        Ok(out)
    }

    pub async fn last_notify_dedup(&self, category: &str, since_ms: i64) -> Result<bool> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT 1 FROM notify_receipts WHERE category = ? AND sent_ms >= ? AND outcome = 'sent' LIMIT 1",
                libsql::params![category, since_ms],
            )
            .await?;
        Ok(rows.next().await?.is_some())
    }

    pub async fn audit(&self, actor: &str, action: &str, detail: &str) -> Result<()> {
        let conn = self.connect().await?;
        conn.execute(
            "INSERT INTO audit(at_ms, actor, action, detail) VALUES (?, ?, ?, ?)",
            libsql::params![now_ms(), actor, action, detail],
        )
        .await?;
        conn.execute(
            "DELETE FROM audit WHERE id NOT IN (SELECT id FROM audit ORDER BY at_ms DESC LIMIT 500)",
            (),
        )
        .await?;
        Ok(())
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
