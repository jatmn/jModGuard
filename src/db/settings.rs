use super::*;

// ---------------------------------------------------------------------------
// settings
// ---------------------------------------------------------------------------

pub(super) fn read_settings_override_conn(
    conn: &impl SqlExecutor,
    guild_id: i64,
) -> Result<Option<GuildSettingsOverride>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT settings FROM guild_settings WHERE guild_id = ?1",
            params![guild_id],
            |r| Ok(r.get(0)?),
        )
        .optional()?;
    let Some(json) = raw else {
        return Ok(None);
    };
    // Corrupt JSON is not a valid override. Runtime reads fail open to
    // inheritance; bot-owned startup healing performs any durable repair plus
    // version/checksum changes atomically.
    match parse_settings_override_json(guild_id, &json) {
        Some(ov) => Ok(Some(ov)),
        None => Ok(None),
    }
}

pub(super) fn effective_settings_conn(
    conn: &impl SqlExecutor,
    guild_id: i64,
    defaults: &GuildSettings,
) -> Result<GuildSettings> {
    let guild = if guild_id == GLOBAL_SCOPE {
        None
    } else {
        read_settings_override_conn(conn, guild_id)?
    };
    let global = read_settings_override_conn(conn, GLOBAL_SCOPE)?;
    Ok(resolve_settings(&guild, &global, defaults))
}

pub(super) fn parent_settings_conn(
    conn: &impl SqlExecutor,
    guild_id: i64,
    defaults: &GuildSettings,
) -> Result<GuildSettings> {
    if guild_id == GLOBAL_SCOPE {
        return Ok(defaults.clone());
    }
    let global = read_settings_override_conn(conn, GLOBAL_SCOPE)?;
    Ok(resolve_settings(&None, &global, defaults))
}

pub(super) fn write_settings_override_conn(
    conn: &impl SqlExecutor,
    guild_id: i64,
    override_: &GuildSettingsOverride,
) -> Result<()> {
    let override_ = normalize_settings_override_for_scope(guild_id, override_.clone());
    if override_.is_empty() {
        if guild_id != GLOBAL_SCOPE {
            conn.execute(
                "DELETE FROM guild_settings WHERE guild_id = ?1",
                params![guild_id],
            )?;
        } else {
            // Global row should exist as an explicit layer; store `{}` rather than delete.
            conn.execute(
                "INSERT INTO guild_settings (guild_id, settings) VALUES (?1, ?2)
                 ON CONFLICT(guild_id) DO UPDATE SET settings = excluded.settings",
                params![guild_id, "{}"],
            )?;
        }
        return Ok(());
    }
    let json = serde_json::to_string(&override_)?;
    conn.execute(
        "INSERT INTO guild_settings (guild_id, settings) VALUES (?1, ?2)
         ON CONFLICT(guild_id) DO UPDATE SET settings = excluded.settings",
        params![guild_id, json],
    )?;
    Ok(())
}

/// Effective + override settings for the WebUI config bundle (one conn read).
pub struct SettingsWebBundle {
    pub settings: GuildSettings,
    pub settings_override: Option<GuildSettingsOverride>,
    pub global_settings: GuildSettings,
    pub global_settings_override: Option<GuildSettingsOverride>,
}

pub(super) fn settings_web_bundle_conn(
    conn: &impl SqlExecutor,
    guild_id: i64,
) -> Result<SettingsWebBundle> {
    let defaults = terminal_defaults_conn(conn)?;
    let global_override = read_settings_override_conn(conn, GLOBAL_SCOPE)?;
    let global_settings = resolve_settings(&None, &global_override, &defaults);
    let (settings_override, settings) = if guild_id == GLOBAL_SCOPE {
        (global_override.clone(), global_settings.clone())
    } else {
        let guild_override = read_settings_override_conn(conn, guild_id)?;
        let settings = resolve_settings(&guild_override, &global_override, &defaults);
        (guild_override, settings)
    };
    Ok(SettingsWebBundle {
        settings,
        settings_override,
        global_settings,
        global_settings_override: global_override,
    })
}

pub async fn get_settings_web_bundle(db: &Db, guild_id: i64) -> Result<SettingsWebBundle> {
    db.with_conn(move |conn| settings_web_bundle_conn(conn, guild_id))
        .await
}

pub async fn get_settings(db: &Db, guild_id: i64) -> Result<GuildSettings> {
    db.with_conn(move |conn| {
        let defaults = terminal_defaults_conn(conn)?;
        effective_settings_conn(conn, guild_id, &defaults)
    })
    .await
}

/// Reads effective settings from a primary-routed snapshot for side-effect
/// reconciliation after independently running bot/API writers.
pub async fn get_settings_primary(db: &Db, guild_id: i64) -> Result<GuildSettings> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_read()?;
        let defaults = terminal_defaults_conn(&tx)?;
        let settings = effective_settings_conn(&tx, guild_id, &defaults)?;
        tx.finish_read()?;
        Ok(settings)
    })
    .await
}

pub async fn get_settings_override(
    db: &Db,
    guild_id: i64,
) -> Result<Option<GuildSettingsOverride>> {
    db.with_conn(move |conn| read_settings_override_conn(conn, guild_id))
        .await
}

/// Parent-layer effective settings for `guild_id` (global → config defaults for
/// guilds; config terminal defaults for global scope).
pub async fn get_parent_settings(db: &Db, guild_id: i64) -> Result<GuildSettings> {
    db.with_conn(move |conn| {
        let defaults = terminal_defaults_conn(conn)?;
        parent_settings_conn(conn, guild_id, &defaults)
    })
    .await
}

/// Effective settings the destination would have after a settings-section copy
/// from `src` (without writing).
pub async fn preview_settings_after_copy(db: &Db, src: i64, dst: i64) -> Result<GuildSettings> {
    db.with_conn(move |conn| {
        let defaults = terminal_defaults_conn(conn)?;
        let dst_parent = parent_settings_conn(conn, dst, &defaults)?;
        let src_ov = read_settings_override_conn(conn, src)?;
        let to_write = match src_ov {
            Some(ov) if !ov.is_empty() => {
                // Normalize against the destination parent while preserving
                // intentional hardcoded-default pins.
                normalize_settings_override_against_parent(&ov, &dst_parent)
            }
            _ => GuildSettingsOverride::default(),
        };
        Ok(to_write.merge_into(&dst_parent))
    })
    .await
}

/// Writes a complete settings blob, then normalizes to a parent-relative
/// partial override (same durable contract as `patch_settings`). Test helper
/// only — production writers must use `patch_settings`.
#[cfg(test)]
pub(crate) async fn upsert_settings(
    db: &Db,
    guild_id: i64,
    settings: &GuildSettings,
) -> Result<()> {
    let override_ = GuildSettingsOverride::from_complete(settings);
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        let defaults = terminal_defaults_conn(&tx)?;
        let parent = parent_settings_conn(&tx, guild_id, &defaults)?;
        let ov = normalize_settings_override_against_parent(&override_, &parent);
        write_settings_override_conn(&tx, guild_id, &ov)?;
        bump_config_version_conn(&tx)?;
        refresh_settings_heal_checksum_conn(&tx)?;
        tx.commit()?;
        Ok(())
    })
    .await
}

/// Patch settings fields into the guild/global override row.
///
/// Each field uses [`crate::types::SettingsPatchField`]:
/// - absent (`None`) — leave unchanged;
/// - `null` (`Some(None)`) — clear override so the parent layer inherits;
/// - value (`Some(Some(v))`) — set override (normalized against parent).
pub async fn patch_settings(
    db: &Db,
    guild_id: i64,
    prefix: crate::types::SettingsPatchField<String>,
    bot_name: crate::types::SettingsPatchField<String>,
    retention_days: crate::types::SettingsPatchField<u32>,
) -> Result<GuildSettings> {
    patch_settings_with_nickname_reconcile(db, guild_id, prefix, bot_name, retention_days, false)
        .await
}

/// Applies one settings patch and, when requested, queues nickname repair in
/// the same transaction. Validation completes before either mutation occurs.
pub async fn patch_settings_with_nickname_reconcile(
    db: &Db,
    guild_id: i64,
    prefix: crate::types::SettingsPatchField<String>,
    bot_name: crate::types::SettingsPatchField<String>,
    retention_days: crate::types::SettingsPatchField<u32>,
    reconcile_nickname: bool,
) -> Result<GuildSettings> {
    if let Some(Some(nickname)) = &bot_name {
        validate_discord_nickname(nickname)?;
    }
    // Trim prefix at the write gate so WebUI / API / !prefix cannot persist
    // leading/trailing whitespace that would break prefix command matching.
    let prefix = match prefix {
        Some(Some(p)) => {
            let trimmed = p.trim().to_string();
            if trimmed.is_empty() {
                anyhow::bail!("prefix cannot be empty");
            }
            Some(Some(trimmed))
        }
        other => other,
    };
    if let Some(Some(days)) = retention_days
        && days > MAX_RETENTION_DAYS
    {
        anyhow::bail!("retention_days must be between 0 and {MAX_RETENTION_DAYS}");
    }
    // No-op patch: return current effective settings without bumping versions.
    let settings_changed = prefix.is_some() || bot_name.is_some() || retention_days.is_some();
    if !settings_changed && !reconcile_nickname {
        return get_settings(db, guild_id).await;
    }
    let nickname_changed = (bot_name.is_some() || reconcile_nickname) && guild_id != GLOBAL_SCOPE;
    let write_guard = Arc::clone(&db.settings_write_lock).lock_owned().await;
    db.with_conn(move |conn| {
        let _write_guard = write_guard;
        let tx = conn.transaction_immediate()?;
        let defaults = terminal_defaults_conn(&tx)?;
        let parent = parent_settings_conn(&tx, guild_id, &defaults)?;
        let mut ov = read_settings_override_conn(&tx, guild_id)?.unwrap_or_default();

        // Apply provided fields, then normalize the *whole* override against
        // parent. Normalizing only patched fields would leave legacy
        // materialized inherited values pinned forever.
        // Heal's ensure_guild-blob clear deliberately does NOT run here — that
        // heuristic belongs only to legacy migration (see heal_settings_override).
        if let Some(p) = prefix {
            ov.prefix = p;
        }
        if let Some(n) = bot_name {
            ov.bot_name = n;
        }
        if let Some(r) = retention_days {
            ov.retention_days = r;
        }
        let ov = normalize_settings_override_against_parent(&ov, &parent);

        if settings_changed {
            write_settings_override_conn(&tx, guild_id, &ov)?;
        }
        if nickname_changed {
            enqueue_nickname_reconcile_conn(&tx, guild_id)?;
        }
        if settings_changed {
            bump_config_version_conn(&tx)?;
        }
        let effective = effective_settings_conn(&tx, guild_id, &defaults)?;
        if settings_changed {
            refresh_settings_heal_checksum_conn(&tx)?;
        }
        tx.commit()?;
        Ok(effective)
    })
    .await
}

// ---------------------------------------------------------------------------
