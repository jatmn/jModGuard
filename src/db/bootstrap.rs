use super::*;

pub(super) fn configure_connection(connection: &Connection, remote: bool) -> Result<()> {
    if remote {
        // Hrana opens a new stream for each transaction, so a connection-level
        // foreign_keys pragma would not carry into transactional writes. The
        // current schema has no FOREIGN KEY clauses; add per-transaction stream
        // setup if a future migration introduces them.
        Ok(())
    } else {
        connection.execute_batch(
            "PRAGMA busy_timeout=5000;
             PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;",
        )
    }
}

pub(super) fn migrate_conn(conn: &mut Connection) -> Result<()> {
    // Migration selection must be authoritative. A lagging remote replica can
    // otherwise report vN-1 after vN committed and make us replay a
    // non-idempotent CREATE/ALTER migration against the primary.
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let target = (i + 1) as i64;
        let tx = conn.transaction_primary_read()?;
        // Recheck while holding the primary write lock. Separate bot
        // processes can otherwise both decide to replay a non-idempotent
        // migration from the same stale pre-lock user_version.
        let version: i64 = tx.query_row("PRAGMA user_version", (), |r| Ok(r.get(0)?))?;
        if version < target {
            tx.execute_batch(sql)
                .with_context(|| format!("migration {target}"))?;
            tx.pragma_update(None, "user_version", target)?;
            tx.commit()?;
            tracing::info!("applied migration {target}");
        } else {
            tx.finish_read()?;
        }
    }
    Ok(())
}

pub(super) fn ensure_schema_current(conn: &impl SqlExecutor) -> Result<()> {
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

pub(super) fn read_bootstrap_defaults_primary(conn: &Connection) -> Result<GuildSettings> {
    let tx = conn.transaction_primary_read()?;
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
    let version: u64 = conn.query_row(
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

pub(super) fn bump_meta_version_conn(conn: &impl SqlExecutor, key: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, '1')
         ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)",
        params![key],
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
        let tx = conn.transaction()?;
        let state = get_version_state_conn(&tx)?;
        tx.finish_read()?;
        Ok(state)
    })
    .await
}

/// Reads both invalidation counters through an IMMEDIATE transaction. Bunny
/// routes write transactions to the primary, so callers can distinguish an
/// authoritative failover rollback from one stale replica observation.
pub async fn get_primary_versions(db: &Db) -> Result<VersionState> {
    db.with_conn(|conn| {
        let tx = conn.transaction_primary_read()?;
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
    db.with_conn_sync(|conn| {
        let tx = conn.transaction_primary_read()?;
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
pub(super) const META_TERMINAL_DEFAULTS: &str = "settings_terminal_defaults";
/// Written only after a bot has completed the entire bootstrap sequence.
/// The API must not treat schema version alone as a readiness signal.
pub(super) const META_BOOTSTRAP_COMPLETE: &str = "bootstrap_complete";
/// Bump when heal logic changes so every DB re-runs migration once.
pub(super) const SETTINGS_HEAL_GENERATION: &str = "3";
pub(super) const META_SETTINGS_HEAL_GENERATION: &str = "settings_heal_generation";
pub(super) const META_SETTINGS_HEAL_CHECKSUM: &str = "settings_heal_checksum";

pub(super) fn meta_string_conn(conn: &impl SqlExecutor, key: &str) -> Result<Option<String>> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
        Ok(r.get(0)?)
    })
    .optional()
    .context("meta lookup failed")
}

pub(super) fn write_meta_string_conn(
    conn: &impl SqlExecutor,
    key: &str,
    value: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
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
    let v: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("corrupt meta.{META_TERMINAL_DEFAULTS}"))?;
    Ok(Some(GuildSettings {
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
    }))
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
        tx.execute(
            "DELETE FROM meta WHERE key = ?1",
            params![META_BOOTSTRAP_COMPLETE],
        )?;
    }
    tx.commit()?;
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
    conn.execute(
        "DELETE FROM meta WHERE key IN (?1, ?2)",
        params![META_SETTINGS_HEAL_GENERATION, META_SETTINGS_HEAL_CHECKSUM],
    )?;
    Ok(())
}

pub(super) fn seed_globals_conn(
    conn: &mut Connection,
    default_prefix: &str,
    retention_days: u32,
    terminal: &GuildSettings,
) -> Result<()> {
    let tx = conn.transaction_immediate()?;
    let conn = &tx;
    // Seed only config-driven fields, then strip anything equal to terminal
    // defaults (not hardcoded `GuildSettings::default()`). Never pin empty
    // bot_name as a global override — that blocked inheritance.
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
    let mut changed = conn.execute(
        "INSERT OR IGNORE INTO guild_settings (guild_id, settings) VALUES (?1, ?2)",
        params![GLOBAL_SCOPE, json],
    )?;
    let rules = crate::defaults::default_rules();
    for (i, rule) in rules.iter().enumerate() {
        changed += conn.execute(
            "INSERT OR IGNORE INTO rules (guild_id, rule_id, data) VALUES (?1, ?2, ?3)",
            params![
                GLOBAL_SCOPE,
                crate::types::RULE_IDS[i],
                serde_json::to_string(rule)?
            ],
        )?;
    }
    let mut commands_changed = 0usize;
    for cmd in crate::defaults::default_commands() {
        if cmd.name == "setname" {
            let mut legacy = cmd.clone();
            legacy.description =
                "Set the bot's nickname in this server (omit / clear / - to reset).".into();
            legacy.response = "Bot nickname set to {name}".into();
            commands_changed += conn.execute(
                "UPDATE commands SET data = ?1
                 WHERE guild_id = ?2 AND name = 'setname' AND data = ?3",
                params![
                    serde_json::to_string(&cmd)?,
                    GLOBAL_SCOPE,
                    serde_json::to_string(&legacy)?
                ],
            )?;
        }
        commands_changed += conn.execute(
            "INSERT OR IGNORE INTO commands (guild_id, name, data) VALUES (?1, ?2, ?3)",
            params![
                GLOBAL_SCOPE,
                cmd.name.to_lowercase(),
                serde_json::to_string(&cmd)?
            ],
        )?;
    }
    for (event, template) in crate::defaults::default_templates() {
        changed += conn.execute(
            "INSERT OR IGNORE INTO templates (guild_id, event_type, template) VALUES (?1, ?2, ?3)",
            params![GLOBAL_SCOPE, event, template],
        )?;
    }
    changed += commands_changed;
    if changed > 0 {
        bump_config_version_conn(conn)?;
    }
    if commands_changed > 0 {
        bump_slash_registry_version_conn(conn)?;
    }
    tx.commit()?;
    Ok(())
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
