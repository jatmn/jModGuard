use super::*;

pub(super) fn configure_connection(
    connection: &Connection,
    remote: bool,
    busy_timeout_ms: u32,
    read_only: bool,
) -> Result<()> {
    if remote {
        // Hrana opens a new stream for each transaction, so a connection-level
        // foreign_keys pragma would not carry into transactional writes. The
        // current schema has no FOREIGN KEY clauses; add per-transaction stream
        // setup if a future migration introduces them. Remote busy timeout is
        // not honored; the client operation deadline is the bound. A read-only
        // client refuses writer-lock transactions before any BEGIN is sent.
        let _ = read_only;
        Ok(())
    } else if read_only {
        connection.execute_batch(&format!(
            "PRAGMA busy_timeout={busy_timeout_ms};
             PRAGMA foreign_keys=ON;
             PRAGMA query_only=ON;"
        ))
    } else {
        connection.execute_batch(&format!(
            "PRAGMA busy_timeout={busy_timeout_ms};
             PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;"
        ))
    }
}

/// Accept an empty database or this lineage, never an unrelated/previous install.
/// Caller supplies a primary transaction so all checks share an authoritative snapshot.
fn ensure_schema_family(conn: &impl SqlExecutor) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", (), |r| Ok(r.get(0)?))?;
    let objects: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE name NOT GLOB 'sqlite_*'",
        (),
        |r| Ok(r.get(0)?),
    )?;
    if version == 0 && objects == 0 {
        return Ok(());
    }
    let has_meta: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='meta'",
        (),
        |r| Ok(r.get(0)?),
    )?;
    let family = if has_meta == 1 {
        meta_string_conn(conn, "schema_family")?
    } else {
        None
    };
    if family.as_deref() != Some(SCHEMA_FAMILY) {
        return Err(UnsupportedSchema(
            "unsupported database schema family; this release requires a fresh empty database or a database initialized by this baseline. Existing data was not adopted".into(),
        ).into());
    }
    if version < 1 {
        return Err(UnsupportedSchema(
            "invalid schema version for an initialized jModGuard database".into(),
        )
        .into());
    }
    if version > MIGRATIONS.len() as i64 {
        return Err(UnsupportedSchema(
            "database schema is newer than this binary; use a compatible release".into(),
        )
        .into());
    }
    Ok(())
}

pub(super) fn migrate_conn(conn: &mut Connection) -> Result<()> {
    // A current database must not open one immediate transaction per migration.
    // Each of those is a primary write lock and several Hrana round trips, and
    // the following seed used to keep another write lock open across the same
    // kind of per-statement loop. Bunny closes that stream with
    // TRANSACTION_TIMEOUT.
    {
        let tx = conn.transaction_primary_read()?;
        ensure_schema_family(&tx)?;
        let version: i64 = tx.query_row("PRAGMA user_version", (), |r| Ok(r.get(0)?))?;
        let current = version >= MIGRATIONS.len() as i64;
        tx.finish_read()?;
        if current {
            return Ok(());
        }
    }
    // Migration selection must be authoritative. A lagging remote replica can
    // otherwise report vN-1 after vN committed and make us replay a
    // non-idempotent CREATE/ALTER migration against the primary.
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let target = (i + 1) as i64;
        let tx = conn.transaction_primary_read()?;
        ensure_schema_family(&tx)?;
        // Recheck while holding the primary write lock. Separate bot
        // processes can otherwise both decide to replay a non-idempotent
        // migration from the same stale pre-lock user_version.
        let version: i64 = tx.query_row("PRAGMA user_version", (), |r| Ok(r.get(0)?))?;
        if version < target {
            // Historical migrations can copy existing rows and build indexes.
            // Reserve their complete conservative rebuild allowance under the
            // same primary lock; a large upgrade must fail before mutation.
            let growth = crate::capacity::allocated_bytes(&tx)?
                .checked_mul(8)
                .and_then(|n| n.checked_add(16 * 1024 * 1024))
                .ok_or_else(|| anyhow::anyhow!("migration capacity bound overflow"))?;
            tx.execute_batch_bounded(sql, growth)
                .with_context(|| format!("migration {target}"))?;
            tx.pragma_update(None, "user_version", target)?;
            // Migrations predate `mutation_receipts`, so their primary-routed
            // transactions cannot use the normal receipt protocol. A lost
            // COMMIT response is nevertheless recoverable: `user_version`
            // advances in the same transaction as the migration SQL. Check a
            // fresh authoritative snapshot before treating that outcome as a
            // failed bootstrap and taking the bot down.
            match tx.commit() {
                Ok(()) => {}
                Err(error) if is_database_commit_indeterminate(&error) => {
                    let verify = conn.transaction_primary_read().and_then(|check| {
                        let applied = check
                            .query_row("PRAGMA user_version", (), |r| Ok(r.get::<i64>(0)?))?
                            >= target;
                        check.finish_read()?;
                        Ok(applied)
                    });
                    match verify {
                        Ok(true) => tracing::warn!(
                            migration = target,
                            "migration commit response was lost; authoritative schema version confirms it applied"
                        ),
                        Ok(false) => return Err(error),
                        Err(verify_error) => {
                            return Err(error.context(format!(
                                "migration {target} commit was indeterminate and authoritative verification failed: {verify_error:#}"
                            )));
                        }
                    }
                }
                Err(error) => return Err(error),
            }
            tracing::info!("applied migration {target}");
        } else {
            tx.finish_read()?;
        }
    }
    Ok(())
}

pub(super) fn ensure_schema_current(conn: &impl SqlExecutor) -> Result<()> {
    ensure_schema_family(conn)?;
    let version: i64 = conn.query_row("PRAGMA user_version", (), |r| Ok(r.get(0)?))?;
    let required = MIGRATIONS.len() as i64;
    if version < required {
        return Err(SchemaNotReady(format!(
            "database schema is behind (user_version={version}, required={required}). \
             Start jmodguard-bot once first so migrations run, then start jmodguard-api."
        ))
        .into());
    }
    // Require durable config_version marker so cross-process invalidation works.
    let has_meta: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='meta'",
        (),
        |r| Ok(r.get(0)?),
    )?;
    if has_meta == 0 {
        return Err(SchemaNotReady(
            "database is missing meta table (config_version). \
             Start jmodguard-bot once so migrations run, then start jmodguard-api."
                .into(),
        )
        .into());
    }
    Ok(())
}

/// Permit a current, completed installation to start for inspection and
/// maintenance while ordinary bootstrap writes are over the growth stop.
/// An incomplete schema or missing persisted defaults still takes the normal
/// migration path, which must refuse unsafe growth.
pub(super) fn bootstrap_at_capacity(conn: &Connection) -> Result<bool> {
    let tx = conn.transaction_primary_read()?;
    let allocated = crate::capacity::allocated_bytes(&tx)?;
    if allocated < conn.capacity_growth_limit() {
        tx.finish_read()?;
        return Ok(false);
    }
    let complete = match ensure_schema_current(&tx).and_then(|_| ensure_bootstrap_complete(&tx)) {
        Ok(()) => read_terminal_defaults_meta(&tx)?.is_some(),
        Err(error) if is_schema_not_ready(&error) => false,
        Err(error) => return Err(error),
    };
    if complete {
        tx.probe_write_permission()
            .map_err(annotate_readonly_token)?;
    }
    tx.finish_read()?;
    Ok(complete)
}

pub(super) fn read_bootstrap_defaults_primary(conn: &Connection) -> Result<GuildSettings> {
    let tx = conn.transaction_primary_snapshot()?;
    ensure_schema_current(&tx)?;
    ensure_bootstrap_complete(&tx)?;
    let defaults = read_terminal_defaults_meta(&tx)?.ok_or_else(|| {
        SchemaNotReady(
            "database is missing meta.settings_terminal_defaults. Start jmodguard-bot once first, then start jmodguard-api."
                .into(),
        )
    })?;
    tx.finish_read()?;
    Ok(defaults)
}

/// Bumps durable `config_version` on an open connection (config writers only).
pub(crate) fn bump_config_version_conn(conn: &impl SqlExecutor) -> Result<()> {
    let version: u64 = conn.query_row_write(
        "INSERT INTO meta (key, value) VALUES ('config_version', '1')
         ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)
         RETURNING CAST(value AS INTEGER)",
        (),
        |row| Ok(row.get::<i64>(0)? as u64),
    )?;
    write_meta_string_conn(
        conn,
        "config_revision",
        &format!("{:032x}", rand::random::<u128>()),
    )?;
    conn.record_config_version(version);
    Ok(())
}

/// Bumps durable `slash_registry_version` when the Discord slash command
/// registry may have changed (commands table writes / command copies).
pub(crate) fn bump_slash_registry_version_conn(conn: &impl SqlExecutor) -> Result<()> {
    bump_meta_version_conn(conn, "slash_registry_version")?;
    write_meta_string_conn(
        conn,
        "slash_registry_revision",
        &format!("{:032x}", rand::random::<u128>()),
    )
}

/// Only for verified removal/closure transactions under reclaim admission.
/// Existing completed schemas have these fixed rows; missing markers refuse
/// the cleanup rather than silently dropping invalidation floors.
pub(super) fn bump_config_version_reclaim_conn(tx: &crate::db_compat::Transaction) -> Result<()> {
    anyhow::ensure!(tx.execute_reclaim_metadata_bounded(
        "UPDATE meta SET value=CAST(CAST(value AS INTEGER)+1 AS TEXT) WHERE key='config_version'",
        (), reclaim_meta_bound(tx)?,
    )? == 1, "config_version marker is missing");
    anyhow::ensure!(
        tx.execute_reclaim_metadata_bounded(
            "UPDATE meta SET value=?1 WHERE key='config_revision'",
            params![format!("{:032x}", rand::random::<u128>())],
            reclaim_meta_bound(tx)?,
        )? == 1,
        "config_revision marker is missing"
    );
    tx.record_config_version(get_config_version_conn(tx)?);
    Ok(())
}

pub(super) fn bump_slash_version_reclaim_conn(tx: &crate::db_compat::Transaction) -> Result<()> {
    anyhow::ensure!(tx.execute_reclaim_metadata_bounded(
        "UPDATE meta SET value=CAST(CAST(value AS INTEGER)+1 AS TEXT) WHERE key='slash_registry_version'",
        (), reclaim_meta_bound(tx)?,
    )? == 1, "slash_registry_version marker is missing");
    anyhow::ensure!(
        tx.execute_reclaim_metadata_bounded(
            "UPDATE meta SET value=?1 WHERE key='slash_registry_revision'",
            params![format!("{:032x}", rand::random::<u128>())],
            reclaim_meta_bound(tx)?,
        )? == 1,
        "slash_registry_revision marker is missing"
    );
    Ok(())
}

pub(super) fn reclaim_meta_bound(tx: &impl SqlExecutor) -> Result<u64> {
    Ok(tx.capacity_page_size()?.saturating_mul(1024))
}

pub(super) fn bump_meta_version_conn(conn: &impl SqlExecutor, key: &str) -> Result<()> {
    conn.execute_rows(
        "INSERT INTO meta (key, value) VALUES (?1, '1')
         ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)",
        params![key],
        1,
    )?;
    Ok(())
}

pub async fn bump_config_version(db: &Db) -> Result<()> {
    db.with_conn(|conn| {
        let tx = conn.transaction_immediate()?;
        bump_config_version_conn(&tx)?;
        tx.commit()
    })
    .await
}

pub(super) fn get_meta_version_conn(conn: &impl SqlExecutor, key: &str) -> Result<u64> {
    let value: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
            Ok(r.get(0)?)
        })
        .optional()?;
    Ok(value.as_deref().unwrap_or("0").parse::<u64>().unwrap_or(0))
}

pub(super) fn get_config_version_conn(conn: &impl SqlExecutor) -> Result<u64> {
    get_meta_version_conn(conn, "config_version")
}

pub(super) fn get_slash_registry_version_conn(conn: &impl SqlExecutor) -> Result<u64> {
    get_meta_version_conn(conn, "slash_registry_version")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionState {
    pub config: u64,
    pub slash: u64,
    pub config_revision: String,
    pub slash_revision: String,
}

pub(super) fn get_version_state_conn(conn: &impl SqlExecutor) -> Result<VersionState> {
    let config = get_config_version_conn(conn)?;
    let slash = get_slash_registry_version_conn(conn)?;
    let config_revision =
        meta_string_conn(conn, "config_revision")?.unwrap_or_else(|| format!("legacy:{config}"));
    let slash_revision = meta_string_conn(conn, "slash_registry_revision")?
        .unwrap_or_else(|| format!("legacy:{slash}"));
    Ok(VersionState {
        config,
        slash,
        config_revision,
        slash_revision,
    })
}

pub async fn get_config_version(db: &Db) -> Result<u64> {
    db.with_conn(|conn| get_config_version_conn(conn)).await
}

/// Sync read for process startup (prime poll baseline before traffic).
pub fn get_config_version_sync(db: &Db) -> Result<u64> {
    db.with_conn_sync(|conn| get_config_version_conn(conn))
}

pub async fn get_slash_registry_version(db: &Db) -> Result<u64> {
    db.with_conn(|conn| get_slash_registry_version_conn(conn))
        .await
}

pub async fn get_version_state(db: &Db) -> Result<VersionState> {
    db.with_conn(|conn| {
        let tx = conn.transaction_consistent_read()?;
        let state = get_version_state_conn(&tx)?;
        tx.finish_read()?;
        Ok(state)
    })
    .await
}

/// Reads both invalidation counters from a deferred primary snapshot. The
/// statement is not read-only, so remote libSQL still routes it to the primary,
/// and it does not take the writer lock.
pub async fn get_primary_versions(db: &Db) -> Result<VersionState> {
    db.with_conn(|conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let state = get_version_state_conn(&tx)?;
        tx.finish_read()?;
        Ok(state)
    })
    .await
}

/// Sync read for process startup (prime slash-registry poll baseline).
pub fn get_slash_registry_version_sync(db: &Db) -> Result<u64> {
    db.with_conn_sync(|conn| get_slash_registry_version_conn(conn))
}

/// Primary-routed startup baseline. A replica can expose schema metadata before
/// the atomically seeded command registry has converged.
pub fn get_primary_versions_sync(db: &Db) -> Result<VersionState> {
    if let Some(path) = db.sqlite_path()
        && take_starting_state_failure(path)
    {
        anyhow::bail!("database is locked");
    }
    db.with_conn_sync(|conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let state = get_version_state_conn(&tx)?;
        tx.finish_read()?;
        Ok(state)
    })
}

/// Durable terminal settings fallback (`bot.default_prefix` / `bot.retention_days`).
///
/// Stored in `meta` so bot and API share one source of truth when the global
/// settings override is empty. Process-local config alone caused split-brain
/// whenever the two processes loaded different config values.
static STARTING_STATE_FAILURES: std::sync::Mutex<Vec<(std::path::PathBuf, u32)>> =
    std::sync::Mutex::new(Vec::new());

/// Fail the next `times` starting-state reads for this database path only.
/// The error is the local startup lock text, so the production retry treats it
/// as retryable. Other databases in the same process are unaffected.
#[doc(hidden)]
pub fn test_fail_starting_state_read(path: &std::path::Path, times: u32) {
    STARTING_STATE_FAILURES
        .lock()
        .expect("starting-state failure lock")
        .push((path.to_path_buf(), times));
}

fn take_starting_state_failure(path: &std::path::Path) -> bool {
    let mut pending = STARTING_STATE_FAILURES
        .lock()
        .expect("starting-state failure lock");
    let Some(index) = pending.iter().position(|(candidate, _)| candidate == path) else {
        return false;
    };
    if pending[index].1 <= 1 {
        pending.remove(index);
    } else {
        pending[index].1 -= 1;
    }
    true
}

const ACTIVE_BOT_LEASE: &str = "active_bot_lease";
const ACTIVE_BOT_GENERATION: &str = "active_bot_generation";
pub const ACTIVE_BOT_LEASE_TTL_MS: i64 = 20_000;

fn valid_bot_instance(instance: &str) -> bool {
    instance.len() == 32 && instance.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn lease_blocks(current: Option<&str>, instance: &str, now_ms: i64) -> bool {
    let Some(value) = current else {
        return false;
    };
    let Some((owner, expires)) = value.rsplit_once(' ') else {
        return false;
    };
    let Ok(expires) = expires.parse::<i64>() else {
        return false;
    };
    owner != instance && expires > now_ms
}

/// The current owner still holds an unexpired row. Expired and foreign rows are fresh claims.
fn lease_is_renewal(current: Option<&str>, instance: &str, now_ms: i64) -> bool {
    let Some(value) = current else {
        return false;
    };
    let Some((owner, expires)) = value.rsplit_once(' ') else {
        return false;
    };
    let Ok(expires) = expires.parse::<i64>() else {
        return false;
    };
    owner == instance && expires > now_ms
}

fn generation_value(raw: Option<&str>) -> Result<i64> {
    let Some(raw) = raw else {
        return Ok(0);
    };
    let value = raw
        .parse::<i64>()
        .context("active bot generation is corrupt")?;
    anyhow::ensure!(value >= 0, "active bot generation is corrupt");
    Ok(value)
}

/// Take or renew the active-bot lease under the writer lock.
///
/// A fresh lease (row absent, expired, or owned by someone else) increments
/// `active_bot_generation` in the same transaction. A renewal by the current
/// owner keeps that generation. `refuse` is checked after the lock is held.
/// When it is set, this does not write, so a shutdown that already started
/// cannot reclaim the lease.
pub async fn claim_active_bot(
    db: &Db,
    instance: &str,
    now_ms: i64,
    refuse: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<bool> {
    anyhow::ensure!(valid_bot_instance(instance), "invalid active-bot instance");
    let instance = instance.to_owned();
    let armed = instance.clone();
    let generation = db
        .with_conn(move |conn| {
            // This transaction writes the lease row. It must commit even when a
            // previous owner is still armed on the pool.
            conn.set_expected_lease_owner(None);
            let mut tx = conn.transaction_primary_read()?;
            if refuse.load(std::sync::atomic::Ordering::Acquire) {
                tx.finish_read()?;
                return Ok(None);
            }
            let current = meta_string_conn(&tx, ACTIVE_BOT_LEASE)?;
            if lease_blocks(current.as_deref(), &instance, now_ms) {
                tx.finish_read()?;
                return Ok(None);
            }
            let renewal = lease_is_renewal(current.as_deref(), &instance, now_ms);
            let stored =
                generation_value(meta_string_conn(&tx, ACTIVE_BOT_GENERATION)?.as_deref())?;
            let generation = if renewal {
                if stored > 0 { stored } else { 1 }
            } else {
                stored
                    .checked_add(1)
                    .context("active bot generation overflow")?
            };
            let expires = now_ms.saturating_add(ACTIVE_BOT_LEASE_TTL_MS);
            tx.ensure_mutation_receipt()?;
            write_meta_string_conn(&tx, ACTIVE_BOT_LEASE, &format!("{instance} {expires}"))?;
            if !renewal || stored == 0 {
                write_meta_string_conn(&tx, ACTIVE_BOT_GENERATION, &generation.to_string())?;
            }
            tx.commit()?;
            Ok(Some(generation))
        })
        .await?;
    let Some(generation) = generation else {
        return Ok(false);
    };
    db.arm_writer_term(&armed, generation);
    Ok(true)
}

/// Renew only an already-armed owner; startup and generation-zero claims keep
/// the interactive claim path. A zero-row CAS means authority has been lost.
pub async fn renew_active_bot(
    db: &Db,
    instance: &str,
    now_ms: i64,
    deadline: std::time::Instant,
    refuse: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<bool> {
    anyhow::ensure!(
        std::time::Instant::now() < deadline,
        "active-bot lease authority window expired"
    );
    anyhow::ensure!(valid_bot_instance(instance), "invalid active-bot instance");
    if db.leased_owner().as_deref() != Some(instance)
        || db
            .leased_generation()
            .is_none_or(|generation| generation <= 0)
    {
        return claim_active_bot(db, instance, now_ms, refuse).await;
    }
    let instance = instance.to_owned();
    let renewed = db
        .with_lease_conn(move |conn| {
            if refuse.load(std::sync::atomic::Ordering::Acquire) {
                return Ok(false);
            }
            let expires = now_ms.saturating_add(ACTIVE_BOT_LEASE_TTL_MS);
            Ok(conn.execute_lease_cas(
                crate::db_compat::ACTIVE_BOT_LEASE_CAS,
                params![
                    format!("{instance} {expires}"),
                    format!("{instance} "),
                    now_ms
                ],
                deadline.saturating_duration_since(std::time::Instant::now()),
            )? == 1)
        })
        .await?;
    // The runtime may poll a ready response before its deadline timer.
    anyhow::ensure!(
        std::time::Instant::now() < deadline,
        "active-bot lease renewal completed too late"
    );
    Ok(renewed)
}

/// Drop the lease only when this instance still owns the row.
///
/// The generation is left in place. The next fresh claim increments it.
pub async fn release_active_bot(db: &Db, instance: &str) -> Result<()> {
    anyhow::ensure!(valid_bot_instance(instance), "invalid active-bot instance");
    let instance = instance.to_owned();
    db.with_conn(move |conn| {
        // The delete removes the row this commit would re-read. Ownership is
        // checked below; the pool stays armed so later commits fail.
        conn.set_expected_lease_owner(None);
        let mut tx = conn.transaction_primary_read()?;
        let current = meta_string_conn(&tx, ACTIVE_BOT_LEASE)?;
        let owned = current
            .as_deref()
            .is_some_and(|value| value.starts_with(&format!("{instance} ")));
        if !owned {
            tx.finish_read()?;
            return Ok(());
        }
        tx.ensure_mutation_receipt()?;
        let owner_prefix = format!("{instance} ");
        tx.execute_rows(
            "DELETE FROM meta WHERE key = ?1 AND substr(value, 1, length(?2)) = ?2",
            params![ACTIVE_BOT_LEASE, owner_prefix],
            1,
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}

pub(super) const META_TERMINAL_DEFAULTS: &str = "settings_terminal_defaults";
/// Written only after a bot has completed the entire bootstrap sequence.
/// The API must not treat schema version alone as a readiness signal.
pub(super) const META_BOOTSTRAP_COMPLETE: &str = "bootstrap_complete";
/// Bump when heal logic changes so every DB re-runs migration once.
pub(super) const SETTINGS_HEAL_GENERATION: &str = "3";
pub(super) const META_SETTINGS_HEAL_GENERATION: &str = "settings_heal_generation";
pub(super) const META_SETTINGS_HEAL_CHECKSUM: &str = "settings_heal_checksum";

pub fn meta_string_conn(conn: &impl SqlExecutor, key: &str) -> Result<Option<String>> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
        Ok(r.get(0)?)
    })
    .optional()
    .context("meta lookup failed")
}

pub fn write_meta_string_conn(conn: &impl SqlExecutor, key: &str, value: &str) -> Result<()> {
    conn.execute_rows(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
        1,
    )?;
    Ok(())
}

pub(super) fn write_terminal_defaults_meta(
    conn: &Connection,
    defaults: &GuildSettings,
) -> Result<()> {
    let tx = conn.transaction_immediate()?;
    let json = serde_json::json!({
        "prefix": defaults.prefix,
        "retention_days": defaults.retention_days,
    });
    let json = json.to_string();
    let prev = meta_string_conn(&tx, META_TERMINAL_DEFAULTS)?;
    write_meta_string_conn(&tx, META_TERMINAL_DEFAULTS, &json)?;
    // Bump only when an existing meta value changes (not first seed).
    if let Some(p) = prev
        && p != json
    {
        bump_config_version_conn(&tx)?;
    }
    tx.commit()?;
    Ok(())
}

pub(super) fn read_terminal_defaults_meta(
    conn: &impl SqlExecutor,
) -> Result<Option<GuildSettings>> {
    let value = meta_string_conn(conn, META_TERMINAL_DEFAULTS)?;
    let Some(raw) = value else {
        return Ok(None);
    };
    Ok(Some(terminal_defaults_from_json(&raw)?))
}

pub(crate) fn terminal_defaults_from_json(raw: &str) -> Result<GuildSettings> {
    let v: serde_json::Value = serde_json::from_str(raw)
        .with_context(|| format!("corrupt meta.{META_TERMINAL_DEFAULTS}"))?;
    Ok(GuildSettings {
        prefix: v
            .get("prefix")
            .and_then(|p| p.as_str())
            .unwrap_or(GuildSettings::default().prefix.as_str())
            .to_string(),
        bot_name: GuildSettings::default().bot_name,
        retention_days: v
            .get("retention_days")
            .and_then(|r| r.as_u64())
            .map(|r| r as u32)
            .unwrap_or(GuildSettings::default().retention_days),
    })
}

/// True when a migrate-open must hide `bootstrap_complete` from the API.
///
/// An already-bootstrapped reconnect must not clear the marker: a later
/// retryable pool failure would otherwise block API start for the whole
/// remote retry window. Transport errors propagate so the retry loop can
/// run without deleting a valid marker.
///
/// The decision is primary-routed. A lagging replica can omit a marker the
/// primary already published; clearing from that stale view would fence a
/// ready database. Writes (`clear_bootstrap_complete`) already go to the
/// primary — the read that authorizes them must too.
pub(super) fn migrate_open_needs_api_fence(conn: &Connection) -> Result<bool> {
    let tx = conn.transaction_primary_read()?;
    let needed = match ensure_schema_current(&tx) {
        Ok(()) => match ensure_bootstrap_complete(&tx) {
            Ok(()) => Ok(false),
            Err(error) if is_schema_not_ready(&error) => Ok(true),
            Err(error) => Err(error),
        },
        Err(error) if is_schema_not_ready(&error) => Ok(true),
        Err(error) => Err(error),
    };
    match tx.finish_read() {
        Ok(()) => needed,
        Err(finish_error) => needed.and(Err(finish_error)),
    }
}

/// Prove a token can mutate. Startup no longer calls this; the first WebUI
/// write fails closed. The query-only regression still uses it.
#[cfg(test)]
pub(super) fn ensure_api_writes_allowed(conn: &Connection) -> Result<()> {
    let tx = conn
        .transaction_primary_read()
        .map_err(annotate_readonly_token)?;
    tx.probe_write_permission()
        .map_err(annotate_readonly_token)?;
    tx.finish_read().map_err(annotate_readonly_token)?;
    Ok(())
}

fn annotate_readonly_token(error: anyhow::Error) -> anyhow::Error {
    if super::is_rejected_remote_auth(&error) {
        return anyhow::Error::new(RemoteTokenCannotWrite(format!("{error:#}")));
    }
    error
}

pub(super) fn ensure_bootstrap_complete(conn: &impl SqlExecutor) -> Result<()> {
    let expected = MIGRATIONS.len().to_string();
    if meta_string_conn(conn, META_BOOTSTRAP_COMPLETE)?.as_deref() != Some(expected.as_str()) {
        return Err(SchemaNotReady(
            "database bootstrap is incomplete. Start jmodguard-bot once first, then start jmodguard-api."
                .into(),
        )
        .into());
    }
    Ok(())
}

pub(super) fn clear_bootstrap_complete(conn: &Connection) -> Result<()> {
    let tx = conn.transaction_primary_read()?;
    let has_meta: i64 = tx.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='meta'",
        (),
        |r| Ok(r.get(0)?),
    )?;
    if has_meta != 0 {
        // This runs before the receipt schema is available on old databases.
        // Use the same bounded, independently reconciled maintenance path as
        // migrations; ordinary reclaim requires a prepared receipt/fence.
        debug_assert_eq!(META_BOOTSTRAP_COMPLETE, "bootstrap_complete");
        tx.execute_batch_bounded(
            "DELETE FROM meta WHERE key = 'bootstrap_complete'",
            tx.capacity_page_size()?.saturating_mul(1024),
        )?;
    }
    match tx.commit() {
        Ok(()) => {}
        Err(error) if is_database_commit_indeterminate(&error) => {
            // This marker deletion is also pre-receipt during an initial
            // bootstrap. Re-read it through a new primary snapshot: absent
            // establishes that a lost response did not leave an API-visible
            // stale-ready marker behind.
            let verify = conn.transaction_primary_read().and_then(|check| {
                let cleared = if has_meta == 0 {
                    true
                } else {
                    meta_string_conn(&check, META_BOOTSTRAP_COMPLETE)?.is_none()
                };
                check.finish_read()?;
                Ok(cleared)
            });
            match verify {
                Ok(true) => tracing::warn!(
                    "bootstrap-marker commit response was lost; authoritative read confirms it cleared"
                ),
                Ok(false) => return Err(error),
                Err(verify_error) => {
                    return Err(error.context(format!(
                        "bootstrap-marker commit was indeterminate and authoritative verification failed: {verify_error:#}"
                    )));
                }
            }
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

/// Terminal defaults for settings resolution — always read from durable meta.
pub(super) fn terminal_defaults_conn(conn: &impl SqlExecutor) -> Result<GuildSettings> {
    read_terminal_defaults_meta(conn)?.ok_or_else(|| {
        anyhow::anyhow!("meta.settings_terminal_defaults is missing; start jmodguard-bot first")
    })
}

/// Deterministic FNV-1a 64-bit mix (stable across processes/restarts).
///
/// `DefaultHasher` is randomly seeded per process, so a checksum written by the
/// bot never matched after an API/bot restart — the heal fast-path was dead.
pub(super) fn fnv1a64_mix(hash: u64, bytes: &[u8]) -> u64 {
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut hash = hash;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

pub(super) fn guild_settings_checksum_conn(conn: &impl SqlExecutor) -> Result<u64> {
    let mut stmt =
        conn.prepare("SELECT guild_id, settings FROM guild_settings ORDER BY guild_id")?;
    let rows = stmt.query_map((), |r| Ok((r.get::<i64>(0)?, r.get::<String>(1)?)))?;
    // FNV-1a offset basis — fixed seed so bot and API agree on the same DB.
    let mut hash = 0xcbf29ce484222325u64;
    for row in rows {
        let (guild_id, settings) = row?;
        hash = fnv1a64_mix(hash, &guild_id.to_le_bytes());
        hash = fnv1a64_mix(hash, settings.as_bytes());
        // Row separator so adjacent (id, json) pairs cannot collide by concatenation.
        hash = fnv1a64_mix(hash, &[0xff]);
    }
    Ok(hash)
}

/// Keep heal fast-path accurate after any committed `guild_settings` mutation.
pub(super) fn refresh_settings_heal_checksum_conn(conn: &impl SqlExecutor) -> Result<()> {
    let checksum = guild_settings_checksum_conn(conn)?;
    write_meta_string_conn(
        conn,
        META_SETTINGS_HEAL_GENERATION,
        SETTINGS_HEAL_GENERATION,
    )?;
    write_meta_string_conn(conn, META_SETTINGS_HEAL_CHECKSUM, &checksum.to_string())?;
    Ok(())
}

/// Runtime settings writers already produce normalized rows. Invalidate the
/// startup-heal fast-path cheaply instead of rescanning every guild's settings
/// while holding the primary writer transaction; the next bot bootstrap will
/// verify/heal once and publish a fresh checksum.
pub(super) fn invalidate_settings_heal_checksum_conn(conn: &impl SqlExecutor) -> Result<()> {
    conn.execute_reclaim(
        "DELETE FROM meta WHERE key IN (?1, ?2)",
        params![META_SETTINGS_HEAL_GENERATION, META_SETTINGS_HEAL_CHECKSUM],
    )?;
    Ok(())
}

const SEED_META_MARKERS: [&str; 4] = [
    "readable_log_defaults_v1",
    "attachment_delete_template_v1",
    "retained_message_templates_v1",
    "retained_bulk_status_template_v1",
];

/// Unix millis of the last gateway IDENTIFY this process was about to send.
/// A restart reads it so a crash loop cannot identify again inside the same
/// session-start window.
pub const META_GATEWAY_IDENTIFY_MS: &str = "gateway_last_identify_ms";

fn sql_text(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push('\'');
        }
        quoted.push(ch);
    }
    quoted.push('\'');
    quoted
}

fn seed_batch_growth(sql: &str) -> u64 {
    (sql.len() as u64).saturating_mul(4).saturating_add(65_536)
}

fn total_changes(conn: &impl SqlExecutor) -> Result<i64> {
    conn.query_row("SELECT total_changes()", (), |row| Ok(row.get(0)?))
}

/// Legacy Stats rows the every-boot upgrade still rewrites after the one-time
/// presentation marker is set. The payloads must match the UPDATE literals.
fn legacy_stats_payloads(shipped: &crate::types::CommandDef) -> Result<(String, String)> {
    let mut without_alias = shipped.clone();
    without_alias.permission = "everyone".into();
    without_alias.description =
        "This server's retained moderation counts (records, warns, bans).".into();
    without_alias.aliases.clear();
    let mut with_alias = without_alias.clone();
    with_alias.aliases = vec!["status".into()];
    Ok((
        serde_json::to_string(&without_alias)?,
        serde_json::to_string(&with_alias)?,
    ))
}

fn legacy_setname_payload(shipped: &crate::types::CommandDef) -> Result<String> {
    let mut legacy = shipped.clone();
    legacy.description =
        "Set the bot's nickname in this server (omit / clear / - to reset).".into();
    legacy.response = "Bot nickname set to {name}".into();
    Ok(serde_json::to_string(&legacy)?)
}

/// True when a migrate-open has nothing left to write. The check is one short
/// primary read. The previous seed held BEGIN IMMEDIATE across every default
/// row, and each statement was its own Hrana round trip.
pub(crate) fn globals_seed_satisfied(conn: &Connection) -> Result<bool> {
    let tx = conn.transaction_primary_read()?;
    let satisfied = globals_seed_satisfied_tx(&tx)?;
    tx.finish_read()?;
    Ok(satisfied)
}

fn globals_seed_satisfied_tx(conn: &impl SqlExecutor) -> Result<bool> {
    for key in SEED_META_MARKERS {
        if meta_string_conn(conn, key)?.as_deref() != Some("1") {
            return Ok(false);
        }
    }
    // Counts are not enough. The pre-batch seed ran INSERT OR IGNORE for every
    // shipped key on each boot, so a deleted default came back and a customized
    // row did not. An extra custom row must not hide a missing shipped key.
    if !shipped_global_keys_present(conn)? {
        return Ok(false);
    }
    let stats = crate::defaults::default_commands()
        .into_iter()
        .find(|cmd| cmd.name == "stats")
        .context("shipped stats command")?;
    let (stats_without_alias, stats_with_alias) = legacy_stats_payloads(&stats)?;
    let legacy_stats: i64 = conn.query_row(
        "SELECT COUNT(*) FROM commands WHERE name = 'stats' AND data IN (?1, ?2)",
        params![stats_without_alias, stats_with_alias],
        |row| Ok(row.get(0)?),
    )?;
    if legacy_stats > 0 {
        return Ok(false);
    }
    let setname = crate::defaults::default_commands()
        .into_iter()
        .find(|cmd| cmd.name == "setname")
        .context("shipped setname command")?;
    let legacy_setname: i64 = conn.query_row(
        "SELECT COUNT(*) FROM commands WHERE guild_id = ?1 AND name = 'setname' AND data = ?2",
        params![GLOBAL_SCOPE, legacy_setname_payload(&setname)?],
        |row| Ok(row.get(0)?),
    )?;
    Ok(legacy_setname == 0)
}

/// True when every shipped global key is present. Payloads are not compared:
/// INSERT OR IGNORE restores a missing key and leaves an edited one in place.
fn shipped_global_keys_present(conn: &impl SqlExecutor) -> Result<bool> {
    let settings: i64 = conn.query_row(
        "SELECT COUNT(*) FROM guild_settings WHERE guild_id = ?1",
        params![GLOBAL_SCOPE],
        |row| Ok(row.get(0)?),
    )?;
    if settings < 1 {
        return Ok(false);
    }
    let rules = count_global_keys(conn, "rules", "rule_id", &crate::types::RULE_IDS)?;
    if rules != crate::types::RULE_IDS.len() as i64 {
        return Ok(false);
    }
    let commands = crate::defaults::default_commands();
    let command_names: Vec<String> = commands.iter().map(|cmd| cmd.name.to_lowercase()).collect();
    let command_refs: Vec<&str> = command_names.iter().map(String::as_str).collect();
    if count_global_keys(conn, "commands", "name", &command_refs)? != command_refs.len() as i64 {
        return Ok(false);
    }
    let templates = crate::defaults::default_templates();
    let events: Vec<&str> = templates.iter().map(|(event, _)| *event).collect();
    Ok(count_global_keys(conn, "templates", "event_type", &events)? == events.len() as i64)
}

fn count_global_keys(
    conn: &impl SqlExecutor,
    table: &str,
    column: &str,
    keys: &[&str],
) -> Result<i64> {
    anyhow::ensure!(
        matches!(
            (table, column),
            ("rules", "rule_id") | ("commands", "name") | ("templates", "event_type")
        ),
        "seed presence check uses a fixed table"
    );
    anyhow::ensure!(!keys.is_empty(), "shipped key list is empty");
    let list = keys
        .iter()
        .map(|key| sql_text(key))
        .collect::<Vec<_>>()
        .join(", ");
    conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM {table} WHERE guild_id = {GLOBAL_SCOPE} AND {column} IN ({list})"
        ),
        (),
        |row| Ok(row.get(0)?),
    )
}

pub(super) fn seed_globals_conn(
    conn: &mut Connection,
    default_prefix: &str,
    retention_days: u32,
    terminal: &GuildSettings,
) -> Result<()> {
    if globals_seed_satisfied(conn)? {
        return Ok(());
    }
    let tx = conn.transaction_immediate()?;
    // One query decides the status-alias collision. The writes below are a
    // single Hrana batch per group, not one round trip per default row.
    let mut stmt = tx.prepare("SELECT data FROM commands")?;
    let rows = stmt.query_map((), |row| Ok(row.get::<String>(0)?))?;
    let mut status_taken = false;
    for row in rows {
        if let Ok(command) = serde_json::from_str::<crate::types::CommandDef>(&row?)
            && command.name != "stats"
            && command.matches("status")
        {
            status_taken = true;
        }
    }
    drop(stmt);
    let (body_sql, command_sql, markers) =
        seed_globals_sql(default_prefix, retention_days, terminal, &tx, status_taken)?;
    let before = total_changes(&tx)?;
    tx.execute_batch_bounded(&body_sql, seed_batch_growth(&body_sql))?;
    let after_body = total_changes(&tx)?;
    tx.execute_batch_bounded(&command_sql, seed_batch_growth(&command_sql))?;
    let after_commands = total_changes(&tx)?;
    // Marker rows are not configuration changes. Write them after the delta so
    // a one-time flag cannot bump config_version by itself or twice.
    let body_changed = after_body > before;
    let commands_changed = after_commands > after_body;
    if body_changed || commands_changed {
        bump_config_version_conn(&tx)?;
    }
    if commands_changed {
        bump_slash_registry_version_conn(&tx)?;
    }
    for key in markers {
        write_meta_string_conn(&tx, key, "1")?;
    }
    tx.commit()?;
    Ok(())
}

fn seed_globals_sql(
    default_prefix: &str,
    retention_days: u32,
    terminal: &GuildSettings,
    conn: &impl SqlExecutor,
    status_taken: bool,
) -> Result<(String, String, Vec<&'static str>)> {
    let mut body = String::new();
    let mut commands = String::new();
    let mut markers = Vec::new();
    let seeded = GuildSettingsOverride {
        prefix: Some(default_prefix.to_string()),
        bot_name: None,
        retention_days: Some(retention_days),
    };
    let global_settings = normalize_settings_override_against_parent(&seeded, terminal);
    let json = if global_settings.is_empty() {
        "{}".to_string()
    } else {
        serde_json::to_string(&global_settings)?
    };
    body.push_str(&format!(
        "INSERT OR IGNORE INTO guild_settings (guild_id, settings) VALUES ({GLOBAL_SCOPE}, {});\n",
        sql_text(&json)
    ));
    for (i, rule) in crate::defaults::default_rules().iter().enumerate() {
        body.push_str(&format!(
            "INSERT OR IGNORE INTO rules (guild_id, rule_id, data) VALUES ({GLOBAL_SCOPE}, {}, {});\n",
            sql_text(crate::types::RULE_IDS[i]),
            sql_text(&serde_json::to_string(rule)?)
        ));
    }
    let upgrade_presentation = meta_string_conn(conn, "readable_log_defaults_v1")?.is_none();
    if upgrade_presentation {
        let current_templates = crate::defaults::default_templates();
        for (event, old) in crate::defaults::legacy_log_templates() {
            let new = current_templates
                .iter()
                .find(|(name, _)| *name == event)
                .context("shipped template")?
                .1;
            body.push_str(&format!(
                "UPDATE templates SET template = {} WHERE event_type = {} AND template = {};\n",
                sql_text(new),
                sql_text(event),
                sql_text(old)
            ));
        }
        let stats = crate::defaults::default_commands()
            .into_iter()
            .find(|cmd| cmd.name == "stats")
            .context("shipped stats command")?;
        let mut legacy = stats.clone();
        legacy.aliases.clear();
        legacy.permission = "everyone".into();
        legacy.description =
            "This server's retained moderation counts (records, warns, bans).".into();
        if !status_taken {
            commands.push_str(&format!(
                "UPDATE commands SET data = {} WHERE name = 'stats' AND data = {};\n",
                sql_text(&serde_json::to_string(&stats)?),
                sql_text(&serde_json::to_string(&legacy)?)
            ));
        }
        markers.push("readable_log_defaults_v1");
    }
    if meta_string_conn(conn, "attachment_delete_template_v1")?.is_none() {
        let new = crate::defaults::default_templates()
            .into_iter()
            .find(|(event, _)| *event == "message_delete")
            .context("shipped message_delete template")?
            .1;
        body.push_str(&format!(
            "UPDATE templates SET template = {} WHERE event_type = 'message_delete' AND template = {};\n",
            sql_text(new),
            sql_text(crate::defaults::PRE_ATTACHMENT_DELETE_TEMPLATE)
        ));
        markers.push("attachment_delete_template_v1");
    }
    if meta_string_conn(conn, "retained_message_templates_v1")?.is_none() {
        for (event, old) in crate::defaults::PRE_RETAINED_MESSAGE_TEMPLATES {
            let new = crate::defaults::default_templates()
                .into_iter()
                .find(|(name, _)| *name == event)
                .context("shipped message template")?
                .1;
            body.push_str(&format!(
                "UPDATE templates SET template = {} WHERE event_type = {} AND template = {};\n",
                sql_text(new),
                sql_text(event),
                sql_text(old)
            ));
        }
        markers.push("retained_message_templates_v1");
    }
    if meta_string_conn(conn, "retained_bulk_status_template_v1")?.is_none() {
        let new = crate::defaults::default_templates()
            .into_iter()
            .find(|(event, _)| *event == "message_bulk_delete")
            .context("shipped bulk delete template")?
            .1;
        body.push_str(&format!(
            "UPDATE templates SET template = {} WHERE event_type = 'message_bulk_delete' AND template = {};\n",
            sql_text(new),
            sql_text(crate::defaults::PRE_BULK_STATUS_TEMPLATE)
        ));
        markers.push("retained_bulk_status_template_v1");
    }
    for mut cmd in crate::defaults::default_commands() {
        if cmd.name == "stats" && status_taken {
            cmd.aliases.clear();
        }
        if cmd.name == "stats" {
            let (without_alias, with_alias) = legacy_stats_payloads(&cmd)?;
            for (old, had_alias) in [(without_alias, false), (with_alias, true)] {
                let mut upgraded = cmd.clone();
                upgraded.aliases = if had_alias {
                    vec!["status".into()]
                } else {
                    cmd.aliases.clone()
                };
                commands.push_str(&format!(
                    "UPDATE commands SET data = {} WHERE name = 'stats' AND data = {};\n",
                    sql_text(&serde_json::to_string(&upgraded)?),
                    sql_text(&old)
                ));
            }
        }
        if cmd.name == "setname" {
            commands.push_str(&format!(
                "UPDATE commands SET data = {} WHERE guild_id = {GLOBAL_SCOPE} AND name = 'setname' AND data = {};\n",
                sql_text(&serde_json::to_string(&cmd)?),
                sql_text(&legacy_setname_payload(&cmd)?)
            ));
        }
        commands.push_str(&format!(
            "INSERT OR IGNORE INTO commands (guild_id, name, data) VALUES ({GLOBAL_SCOPE}, {}, {});\n",
            sql_text(&cmd.name.to_lowercase()),
            sql_text(&serde_json::to_string(&cmd)?)
        ));
    }
    for (event, template) in crate::defaults::default_templates() {
        body.push_str(&format!(
            "INSERT OR IGNORE INTO templates (guild_id, event_type, template) VALUES ({GLOBAL_SCOPE}, {}, {});\n",
            sql_text(event),
            sql_text(template)
        ));
    }
    Ok((body, commands, markers))
}

pub async fn gateway_last_identify_ms(db: &Db) -> Result<Option<i64>> {
    db.with_conn(|conn| {
        let tx = conn.transaction_primary_read()?;
        let value = meta_string_conn(&tx, META_GATEWAY_IDENTIFY_MS)?
            .and_then(|raw| raw.parse::<i64>().ok());
        tx.finish_read()?;
        Ok(value)
    })
    .await
}

pub async fn record_gateway_identify_ms(db: &Db, millis: i64) -> Result<()> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        write_meta_string_conn(&tx, META_GATEWAY_IDENTIFY_MS, &millis.to_string())?;
        tx.commit()?;
        Ok(())
    })
    .await
}

/// Strip override fields that equal the parent layer (global → hardcoded defaults).
///
/// This is the durable storage contract for settings: a guild row must only
/// retain intentional differences. Identical fields re-enable inheritance.
pub(super) fn normalize_settings_override_against_parent(
    ov: &GuildSettingsOverride,
    parent: &GuildSettings,
) -> GuildSettingsOverride {
    GuildSettingsOverride {
        prefix: ov.prefix.as_ref().filter(|p| *p != &parent.prefix).cloned(),
        bot_name: ov
            .bot_name
            .as_ref()
            .filter(|n| *n != &parent.bot_name)
            .cloned(),
        retention_days: ov.retention_days.filter(|&r| r != parent.retention_days),
    }
}

/// `bot_name` is a Discord guild nickname, never a global default. Keep this
/// invariant in the data layer as well as the HTTP layer: settings can also be
/// copied or written by command/runtime paths that do not pass through the API.
pub(super) fn normalize_settings_override_for_scope(
    guild_id: i64,
    mut override_: GuildSettingsOverride,
) -> GuildSettingsOverride {
    if guild_id == GLOBAL_SCOPE {
        override_.bot_name = None;
    }
    override_
}

/// Normalize a legacy settings row against its parent layer.
///
/// Legacy rows contain no provenance: a complete hardcoded-default blob can
/// be either an old automatic `ensure_guild` seed or an intentional operator
/// override. Preserve that ambiguity rather than deleting a user setting.
pub(super) fn heal_settings_override(
    ov: &GuildSettingsOverride,
    parent: &GuildSettings,
) -> GuildSettingsOverride {
    normalize_settings_override_against_parent(ov, parent)
}

/// Parse a stored settings JSON into an override, or `None` when corrupt.
///
/// Corrupt rows are not valid overrides: callers clear them so inheritance
/// resumes instead of leaving `get_settings` permanently broken.
pub(super) fn parse_settings_override_json(
    guild_id: i64,
    json: &str,
) -> Option<GuildSettingsOverride> {
    match serde_json::from_str::<GuildSettingsOverride>(json) {
        Ok(ov) => Some(ov),
        Err(e) => {
            tracing::warn!(
                "guild_settings row guild_id={guild_id} has corrupt JSON ({e}); \
                 clearing so inheritance can resume"
            );
            None
        }
    }
}

/// One-time (idempotent) settings migration run on bot migrate-open / API open:
/// convert legacy full blobs into parent-relative partial overrides.
///
/// Root causes addressed:
/// - `ensure_guild` seeded full blobs, but those are indistinguishable from
///   intentional legacy operator settings. Ambiguous differences are retained.
/// - Old writers stored *effective* settings as guild rows. Fields equal to
///   the parent layer are stripped so only intentional differences remain.
/// - Global rows were left unmigrated, so redundant full blobs pinned
///   hardcoded defaults at the global layer forever.
/// - Unparseable JSON was skipped, leaving `get_settings` broken for that
///   guild; corrupt rows are cleared instead.
///
/// Order: migrate **global first** (parent = config terminal defaults), then
/// guild rows against the healed global. When any row changes, bumps
/// `config_version` so an already-running API process invalidates its cache.
pub(super) fn heal_default_guild_settings_conn(
    conn: &mut Connection,
    defaults: &GuildSettings,
) -> Result<()> {
    let checksum = guild_settings_checksum_conn(conn)?;
    let stored_gen = meta_string_conn(conn, META_SETTINGS_HEAL_GENERATION)?;
    let stored_checksum =
        meta_string_conn(conn, META_SETTINGS_HEAL_CHECKSUM)?.and_then(|s| s.parse::<u64>().ok());
    if stored_gen.as_deref() == Some(SETTINGS_HEAL_GENERATION) && stored_checksum == Some(checksum)
    {
        return Ok(());
    }

    let tx = conn.transaction_immediate()?;
    let mut changed = 0usize;

    // 1) Global layer first — parent is config terminal defaults.
    let global_raw: Option<String> = tx
        .query_row(
            "SELECT settings FROM guild_settings WHERE guild_id = ?1",
            params![GLOBAL_SCOPE],
            |r| Ok(r.get(0)?),
        )
        .optional()?;
    if let Some(json) = global_raw {
        match parse_settings_override_json(GLOBAL_SCOPE, &json) {
            Some(ov) => {
                let normalized = normalize_settings_override_for_scope(
                    GLOBAL_SCOPE,
                    heal_settings_override(&ov, defaults),
                );
                if normalized != ov {
                    write_settings_override_conn(&tx, GLOBAL_SCOPE, &normalized)?;
                    changed += 1;
                }
            }
            None => {
                write_settings_override_conn(&tx, GLOBAL_SCOPE, &GuildSettingsOverride::default())?;
                changed += 1;
            }
        }
    }

    // 2) Guild rows — parent from (possibly healed) global + terminal defaults.
    let parent = {
        let global = read_settings_override_conn(&tx, GLOBAL_SCOPE)?;
        resolve_settings(&None, &global, defaults)
    };
    let mut stmt =
        tx.prepare("SELECT guild_id, settings FROM guild_settings WHERE guild_id != ?1")?;
    let rows: Vec<(i64, String)> = stmt
        .query_map(params![GLOBAL_SCOPE], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    drop(stmt);

    for (guild_id, json) in rows {
        match parse_settings_override_json(guild_id, &json) {
            Some(ov) => {
                let normalized = heal_settings_override(&ov, &parent);
                if normalized == ov {
                    continue;
                }
                write_settings_override_conn(&tx, guild_id, &normalized)?;
                changed += 1;
            }
            None => {
                // Corrupt guild override → delete (inherit global).
                write_settings_override_conn(&tx, guild_id, &GuildSettingsOverride::default())?;
                changed += 1;
            }
        }
    }

    if changed > 0 {
        bump_config_version_conn(&tx)?;
        tracing::info!(
            "normalized {changed} guild_settings row(s) to partial overrides \
             (inherited fields stripped; ambiguous legacy overrides retained; \
             bumped config_version)"
        );
    }
    tx.commit()?;

    refresh_settings_heal_checksum_conn(conn)?;
    Ok(())
}
