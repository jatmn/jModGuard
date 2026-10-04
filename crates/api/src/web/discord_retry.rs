//! Discord OAuth Retry-After deadlines in the existing core `meta` table.
//! One row, no new schema. A restart reads it before the next guild check.

use crate::db::{Db, SqlExecutor};
use anyhow::{Context as _, Result};
use std::collections::BTreeMap;

pub(crate) const META_KEY: &str = "discord_oauth_retry_until";
const MAX_BYTES: usize = 8192;

pub(crate) fn durable_key(key: &str) -> bool {
    if key == "global" || key == "token" {
        return true;
    }
    let Some(rest) = key.strip_prefix("g:").or_else(|| key.strip_prefix("m:")) else {
        return false;
    };
    !rest.is_empty() && rest.len() <= 20 && rest.bytes().all(|byte| byte.is_ascii_digit())
}

pub(crate) fn parse_deadlines(raw: &str) -> BTreeMap<String, i64> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return BTreeMap::new();
    };
    let Some(object) = value.as_object() else {
        return BTreeMap::new();
    };
    let mut deadlines = BTreeMap::new();
    for (key, deadline) in object {
        if !durable_key(key) {
            continue;
        }
        let Some(millis) = deadline.as_i64().filter(|millis| *millis > 0) else {
            continue;
        };
        deadlines.insert(key.clone(), millis);
    }
    deadlines
}

pub(crate) fn load_deadlines(db: &Db) -> Result<BTreeMap<String, i64>> {
    // `Db::with_conn_sync` blocks on the database runtime. That panics when
    // this process is already inside the API runtime, so a nested caller
    // loads from a thread that is not on that runtime.
    if tokio::runtime::Handle::try_current().is_ok() {
        return std::thread::scope(|scope| {
            scope
                .spawn(|| load_deadlines_sync(db))
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        });
    }
    load_deadlines_sync(db)
}

pub(crate) fn load_deadlines_sync(db: &Db) -> Result<BTreeMap<String, i64>> {
    let raw = db.with_conn_sync(|conn| {
        let value: String = conn
            .query_row(
                "SELECT COALESCE((SELECT value FROM meta WHERE key=?1), '{}')",
                crate::db::params![META_KEY],
                |row| Ok(row.get(0)?),
            )
            .context("discord retry deadline lookup failed")?;
        Ok(value)
    })?;
    Ok(parse_deadlines(&raw))
}

pub(crate) async fn store_deadlines(db: &Db, json: String) -> Result<()> {
    anyhow::ensure!(
        json.len() <= MAX_BYTES,
        "discord retry deadline record exceeds {MAX_BYTES} bytes"
    );
    db.with_conn(move |conn| {
        conn.execute_bounded(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            crate::db::params![META_KEY, json],
            16 * 1024,
        )?;
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadlines_round_trip_through_meta_and_ignore_junk() {
        let path =
            std::env::temp_dir().join(format!("discord_retry_{:032x}.db", rand::random::<u128>()));
        let db = Db::open(&path, 2).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            store_deadlines(
                &db,
                r#"{"g:7":1710000000000,"token":1710000001000,"nope":5,"m:bad":9}"#.into(),
            )
            .await
            .unwrap();
        });
        let loaded = load_deadlines_sync(&Db::open(&path, 2).unwrap()).unwrap();
        assert_eq!(loaded.get("g:7"), Some(&1_710_000_000_000));
        assert_eq!(loaded.get("token"), Some(&1_710_000_001_000));
        assert!(!loaded.contains_key("nope"));
        assert!(!loaded.contains_key("m:bad"));
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn missing_meta_row_is_an_empty_deadline_map() {
        let path = std::env::temp_dir().join(format!(
            "discord_retry_empty_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        assert!(load_deadlines_sync(&db).unwrap().is_empty());
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}
