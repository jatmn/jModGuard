use super::*;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

pub(super) fn now_str() -> String {
    Utc::now().to_rfc3339()
}

fn read_json<T: DeserializeOwned>(data: &str, what: &str) -> Result<T> {
    serde_json::from_str(data).with_context(|| format!("corrupt JSON in {what}"))
}

/// Resolves a per-guild row against the global row, falling back to `default`.
pub(crate) fn resolve<T: Clone>(guild_row: &Option<T>, global_row: &Option<T>, default: &T) -> T {
    guild_row
        .clone()
        .or_else(|| global_row.clone())
        .unwrap_or_else(|| default.clone())
}

// ---------------------------------------------------------------------------
// generic scope row helpers
// ---------------------------------------------------------------------------

pub(crate) async fn load_rows<T: DeserializeOwned + Send + 'static>(
    db: &Db,
    guild_id: i64,
    table: &'static str,
    key_col: &'static str,
    key_value: Option<&str>,
) -> Result<Vec<(String, T)>> {
    let key_value = key_value.map(|s| s.to_string());
    db.with_conn(move |conn| {
        let sql = if key_value.is_some() {
            format!("SELECT {key_col}, data FROM {table} WHERE guild_id = ?1 AND {key_col} = ?2")
        } else {
            format!("SELECT {key_col}, data FROM {table} WHERE guild_id = ?1")
        };
        let mut stmt = conn.prepare(&sql)?;
        let mut args: Vec<Value> = vec![guild_id.into()];
        if let Some(kv) = &key_value {
            args.push(kv.to_string().into());
        }
        let rows = stmt.query_map(params_from_iter(args), |r| {
            Ok((r.get::<String>(0)?, r.get::<String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (k, v) = row?;
            out.push((k, read_json(&v, table)?));
        }
        Ok(out)
    })
    .await
}

async fn upsert_row<T: Serialize + Send + 'static>(
    db: &Db,
    table: &'static str,
    key_cols: &'static str,
    key_params: Vec<Value>,
    data_col: &'static str,
    data: T,
    bump_slash_registry: bool,
) -> Result<()> {
    let json = serde_json::to_string(&data)?;
    db.with_conn(move |conn| {
        let keys = key_params
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let n = key_params.len() + 1;
        let tx = conn.transaction_immediate()?;
        tx.execute(
            &format!(
                "INSERT INTO {table} ({key_cols}, {data_col}) VALUES ({keys}, ?{n})
                 ON CONFLICT({key_cols}) DO UPDATE SET {data_col} = excluded.{data_col}"
            ),
            params_from_iter(key_params.into_iter().chain(std::iter::once(json.into()))),
        )?;
        bump_config_version_conn(&tx)?;
        if bump_slash_registry {
            bump_slash_registry_version_conn(&tx)?;
        }
        tx.commit()?;
        Ok(())
    })
    .await
}

async fn delete_row(
    db: &Db,
    table: &'static str,
    key_cols: &'static str,
    key_params: Vec<Value>,
    bump_slash_registry: bool,
) -> Result<()> {
    db.with_conn(move |conn| {
        let keys = key_params
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let tx = conn.transaction_immediate()?;
        let n = tx.execute(
            &format!("DELETE FROM {table} WHERE ({key_cols}) = ({keys})"),
            params_from_iter(key_params),
        )?;
        // No-op deletes must not bump versions — that forced cross-process
        // cache invalidation and slash re-sync sweeps for already-gone rows.
        if n > 0 {
            bump_config_version_conn(&tx)?;
            if bump_slash_registry {
                bump_slash_registry_version_conn(&tx)?;
            }
        }
        tx.commit()?;
        Ok(())
    })
    .await
}
// rules
// ---------------------------------------------------------------------------

pub async fn list_rules(db: &Db, guild_id: i64) -> Result<Vec<(String, RuleDef)>> {
    load_rows(db, guild_id, "rules", "rule_id", None).await
}

pub async fn get_rule(db: &Db, guild_id: i64, rule_id: &str) -> Result<Option<RuleDef>> {
    let rows: Vec<(String, RuleDef)> =
        load_rows(db, guild_id, "rules", "rule_id", Some(rule_id)).await?;
    Ok(rows.into_iter().next().map(|(_, r)| r))
}

pub async fn upsert_rule(db: &Db, guild_id: i64, rule_id: &str, rule: &RuleDef) -> Result<()> {
    upsert_row(
        db,
        "rules",
        "guild_id, rule_id",
        vec![guild_id.into(), rule_id.to_string().into()],
        "data",
        rule.clone(),
        false,
    )
    .await
}

pub async fn delete_rule(db: &Db, guild_id: i64, rule_id: &str) -> Result<()> {
    delete_row(
        db,
        "rules",
        "guild_id, rule_id",
        vec![guild_id.into(), rule_id.to_string().into()],
        false,
    )
    .await
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

pub async fn list_commands(db: &Db, guild_id: i64) -> Result<Vec<(String, CommandDef)>> {
    load_rows(db, guild_id, "commands", "name", None).await
}

pub async fn get_command_row(db: &Db, guild_id: i64, name: &str) -> Result<Option<CommandDef>> {
    let rows: Vec<(String, CommandDef)> =
        load_rows(db, guild_id, "commands", "name", Some(name)).await?;
    Ok(rows.into_iter().next().map(|(_, c)| c))
}

pub async fn upsert_command(db: &Db, guild_id: i64, name: &str, cmd: &CommandDef) -> Result<()> {
    upsert_row(
        db,
        "commands",
        "guild_id, name",
        vec![guild_id.into(), name.to_string().into()],
        "data",
        cmd.clone(),
        true,
    )
    .await
}

pub async fn delete_command(db: &Db, guild_id: i64, name: &str) -> Result<()> {
    delete_row(
        db,
        "commands",
        "guild_id, name",
        vec![guild_id.into(), name.to_string().into()],
        true,
    )
    .await
}

// ---------------------------------------------------------------------------
// templates
// ---------------------------------------------------------------------------

pub async fn list_templates(db: &Db, guild_id: i64) -> Result<Vec<(String, String)>> {
    db.with_conn(move |conn| {
        let mut stmt =
            conn.prepare("SELECT event_type, template FROM templates WHERE guild_id = ?1")?;
        let rows = stmt.query_map(params![guild_id], |r| {
            Ok((r.get::<String>(0)?, r.get::<String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
    .await
}

pub async fn upsert_template(db: &Db, guild_id: i64, event: &str, template: &str) -> Result<()> {
    let (event, template) = (event.to_string(), template.to_string());
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        tx.execute(
            "INSERT INTO templates (guild_id, event_type, template) VALUES (?1, ?2, ?3)
             ON CONFLICT(guild_id, event_type) DO UPDATE SET template = excluded.template",
            params![guild_id, event, template],
        )?;
        bump_config_version_conn(&tx)?;
        tx.commit()?;
        Ok(())
    })
    .await
}

pub async fn delete_template(db: &Db, guild_id: i64, event: &str) -> Result<()> {
    delete_row(
        db,
        "templates",
        "guild_id, event_type",
        vec![guild_id.into(), event.to_string().into()],
        false,
    )
    .await
}

pub async fn get_template_row(db: &Db, guild_id: i64, event: &str) -> Result<Option<String>> {
    let (guild_id, event) = (guild_id, event.to_string());
    db.with_conn(move |conn| {
        conn.query_row(
            "SELECT template FROM templates WHERE guild_id = ?1 AND event_type = ?2",
            params![guild_id, event],
            |r| Ok(r.get(0)?),
        )
        .optional()
    })
    .await
}

// ---------------------------------------------------------------------------
// log channels
// ---------------------------------------------------------------------------

pub async fn list_log_channels(db: &Db, guild_id: i64) -> Result<Vec<(String, i64)>> {
    db.with_conn(move |conn| {
        let mut stmt =
            conn.prepare("SELECT event_type, channel_id FROM log_channels WHERE guild_id = ?1")?;
        let rows = stmt.query_map(params![guild_id], |r| {
            Ok((r.get::<String>(0)?, r.get::<i64>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
    .await
}

pub async fn upsert_log_channel(
    db: &Db,
    guild_id: i64,
    event: &str,
    channel_id: i64,
) -> Result<()> {
    if channel_id <= 0 {
        anyhow::bail!("channel_id must be a positive Discord snowflake");
    }
    let event = event.to_string();
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        tx.execute(
            "INSERT INTO log_channels (guild_id, event_type, channel_id) VALUES (?1, ?2, ?3)
             ON CONFLICT(guild_id, event_type) DO UPDATE SET channel_id = excluded.channel_id",
            params![guild_id, event, channel_id],
        )?;
        bump_config_version_conn(&tx)?;
        tx.commit()?;
        Ok(())
    })
    .await
}

pub async fn delete_log_channel(db: &Db, guild_id: i64, event: &str) -> Result<()> {
    delete_row(
        db,
        "log_channels",
        "guild_id, event_type",
        vec![guild_id.into(), event.to_string().into()],
        false,
    )
    .await
}

// ---------------------------------------------------------------------------
// badwords
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Badword {
    pub id: i64,
    pub guild_id: i64,
    pub pattern: String,
    pub note: Option<String>,
}

/// API/UI view of a badword with compile status (legacy rows may be invalid).
#[derive(Debug, Clone, serde::Serialize)]
pub struct BadwordView {
    pub id: i64,
    #[serde(serialize_with = "crate::ids::ser_i64_str")]
    pub guild_id: i64,
    pub pattern: String,
    pub note: Option<String>,
    /// False when the pattern fails `compile_user_regex` — not enforced by the bot.
    pub valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Badword {
    /// Annotates compile status so the WebUI can flag legacy/invalid rows.
    pub fn with_status(&self) -> BadwordView {
        match crate::normalize::compile_user_regex(&self.pattern) {
            Ok(_) => BadwordView {
                id: self.id,
                guild_id: self.guild_id,
                pattern: self.pattern.clone(),
                note: self.note.clone(),
                valid: true,
                error: None,
            },
            Err(e) => BadwordView {
                id: self.id,
                guild_id: self.guild_id,
                pattern: self.pattern.clone(),
                note: self.note.clone(),
                valid: false,
                error: Some(e),
            },
        }
    }
}

pub async fn list_badwords(db: &Db, guild_id: i64) -> Result<Vec<Badword>> {
    db.with_conn(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, guild_id, pattern, note FROM badwords
             WHERE guild_id = ?1 OR guild_id = ?2 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![guild_id, GLOBAL_SCOPE], |r| {
            Ok(Badword {
                id: r.get(0)?,
                guild_id: r.get(1)?,
                pattern: r.get(2)?,
                note: r.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
    .await
}

pub async fn add_badword(db: &Db, guild_id: i64, pattern: &str, note: Option<&str>) -> Result<i64> {
    // Single gate for every writer (API, future commands) — reject before INSERT.
    crate::normalize::compile_user_regex(pattern).map_err(|e| anyhow::anyhow!(e))?;
    let (pattern, note) = (pattern.to_string(), note.map(|s| s.to_string()));
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        tx.execute(
            "INSERT INTO badwords (guild_id, pattern, note) VALUES (?1, ?2, ?3)",
            params![guild_id, pattern, note],
        )?;
        let id = tx.last_insert_rowid();
        bump_config_version_conn(&tx)?;
        tx.commit()?;
        Ok(id)
    })
    .await
}

/// Deletes a badword only when `id` belongs to the exact `guild_id` scope.
/// Callers that want to delete a global row must pass `GLOBAL_SCOPE` and
/// enforce owner auth at the API layer.
pub async fn get_badword(db: &Db, id: i64) -> Result<Option<Badword>> {
    db.with_conn(move |conn| {
        conn.query_row(
            "SELECT id, guild_id, pattern, note FROM badwords WHERE id = ?1",
            params![id],
            |r| {
                Ok(Badword {
                    id: r.get(0)?,
                    guild_id: r.get(1)?,
                    pattern: r.get(2)?,
                    note: r.get(3)?,
                })
            },
        )
        .optional()
    })
    .await
}

pub async fn delete_badword(db: &Db, id: i64, guild_id: i64) -> Result<()> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        let n = tx.execute(
            "DELETE FROM badwords WHERE id = ?1 AND guild_id = ?2",
            params![id, guild_id],
        )?;
        if n == 0 {
            // Same public wording as API denials — never leak guild/scope detail.
            anyhow::bail!("badword not found");
        }
        bump_config_version_conn(&tx)?;
        tx.commit()?;
        Ok(())
    })
    .await
}

/// Result of `copy_guild_config` — sections applied plus patterns skipped.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CopyResult {
    pub copied: Vec<&'static str>,
    /// Invalid/legacy badword patterns omitted from the destination replacement set.
    /// Only set when the badwords section succeeded (validated replace committed).
    pub skipped_badwords: usize,
    /// True when logchannels were requested but skipped because Discord channel
    /// IDs are not portable across guilds (or to/from global scope).
    pub skipped_logchannels: bool,
}

/// One validated badword row ready to insert (pattern + optional note).
type BadwordReplacement = (String, Option<String>);

/// Builds the enforceable replacement set for a badwords copy.
///
/// Contract (validate-then-replace — never DELETE-then-filter):
/// - Empty source → empty replacement (intentional clear of destination).
/// - Source has rows but none compile → `Err` (caller must leave destination untouched).
/// - Otherwise → only patterns that pass `compile_user_regex`.
fn prepare_badword_replacement(
    rows: Vec<BadwordReplacement>,
) -> Result<(Vec<BadwordReplacement>, usize)> {
    let source_count = rows.len();
    let mut valid = Vec::with_capacity(source_count);
    let mut skipped = 0usize;
    for (pattern, note) in rows {
        if let Err(e) = crate::normalize::compile_user_regex(&pattern) {
            tracing::warn!("copy_guild_config: rejecting badword `{pattern}`: {e}");
            skipped += 1;
            continue;
        }
        valid.push((pattern, note));
    }
    if source_count > 0 && valid.is_empty() {
        anyhow::bail!(
            "badwords copy refused: {skipped} source pattern(s) failed validation and none \
             were enforceable; destination left unchanged. Fix or delete invalid patterns \
             on the source server first."
        );
    }
    Ok((valid, skipped))
}

fn section_wanted(sections: &[String], name: &str) -> bool {
    sections.iter().any(|s| s == name || s == "all")
}

/// Read-only checks for copy refuse conditions (no writes).
///
/// Validate known copy refusal conditions before starting the write.
fn preflight_copy_guild_config_conn(
    conn: &impl SqlExecutor,
    src: i64,
    dst: i64,
    sections: &[String],
) -> Result<()> {
    if section_wanted(sections, "badwords") {
        let mut stmt = conn.prepare("SELECT pattern, note FROM badwords WHERE guild_id = ?1")?;
        let rows: Vec<(String, Option<String>)> = stmt
            .query_map(params![src], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let _ = prepare_badword_replacement(rows)?;
    }

    if section_wanted(sections, "logchannels") && src != dst {
        let only_logchannels = sections.iter().all(|s| s == "logchannels");
        if only_logchannels {
            anyhow::bail!(
                "log channel IDs are server-specific and cannot be copied \
                 between servers; set them on the destination after copying \
                 other sections"
            );
        }
    }
    Ok(())
}

/// Public preflight for WebUI/API: refuse known hard-fail copy cases early.
pub async fn preflight_copy_guild_config(
    db: &Db,
    src: i64,
    dst: i64,
    sections: Vec<String>,
) -> Result<()> {
    db.with_conn(move |conn| preflight_copy_guild_config_conn(conn, src, dst, &sections))
        .await
}

/// Copies selected config sections from `src` to `dst` in one transaction
/// with a single `config_version` bump (avoids mid-copy cache snapshots).
///
/// Always re-runs [`preflight_copy_guild_config`] inside this call so a
/// concurrent source change between an earlier WebUI preflight and this write
/// cannot skip validation. Callers may still preflight earlier for fast errors.
///
/// Badwords use validate-then-atomic-replace: the destination is only cleared
/// after a usable replacement set is prepared. If every source pattern fails
/// validation, the whole transaction fails and destination badwords are preserved.
pub async fn copy_guild_config(
    db: &Db,
    src: i64,
    dst: i64,
    sections: Vec<String>,
) -> Result<CopyResult> {
    let write_guard = Arc::clone(&db.settings_write_lock).lock_owned().await;
    db.with_conn(move |conn| {
        let _write_guard = write_guard;
        let tx = conn.transaction_immediate()?;
        let defaults = terminal_defaults_conn(&tx)?;
        // Always re-validate in the same primary snapshot as the write.
        preflight_copy_guild_config_conn(&tx, src, dst, &sections)?;
        let mut result = CopyResult::default();
        let wants = |name: &str| section_wanted(&sections, name);
        let copied_settings = wants("settings");

        if copied_settings {
            // Copy the source's explicit override, then normalize against the
            // *destination* parent so inherited fields are not re-pinned.
            // Do not use heal_settings_override: that ensure_guild-blob clear
            // is migrate-only. Writers (patch / copy) preserve intentional
            // hardcoded-default pins when they differ from the parent.
            let src_ov = read_settings_override_conn(&tx, src)?;
            let dst_parent = parent_settings_conn(&tx, dst, &defaults)?;
            let to_write = match src_ov {
                Some(ov) if !ov.is_empty() => {
                    normalize_settings_override_against_parent(&ov, &dst_parent)
                }
                _ => GuildSettingsOverride::default(),
            };
            let to_write = normalize_settings_override_for_scope(dst, to_write);
            let effective_nickname = to_write.bot_name.as_deref().unwrap_or(&dst_parent.bot_name);
            validate_discord_nickname(effective_nickname)?;
            write_settings_override_conn(&tx, dst, &to_write)?;
            if dst != GLOBAL_SCOPE {
                enqueue_nickname_reconcile_conn(&tx, dst)?;
            }
            result.copied.push("settings");
        }

        if wants("rules") {
            let mut stmt = tx.prepare("SELECT rule_id, data FROM rules WHERE guild_id = ?1")?;
            let rows: Vec<(String, String)> = stmt
                .query_map(params![src], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            tx.execute("DELETE FROM rules WHERE guild_id = ?1", params![dst])?;
            for (rid, data) in rows {
                tx.execute(
                    "INSERT INTO rules (guild_id, rule_id, data) VALUES (?1, ?2, ?3)
                     ON CONFLICT(guild_id, rule_id) DO UPDATE SET data = excluded.data",
                    params![dst, rid, data],
                )?;
            }
            result.copied.push("rules");
        }

        if wants("badwords") {
            let mut stmt = tx.prepare("SELECT pattern, note FROM badwords WHERE guild_id = ?1")?;
            let rows: Vec<(String, Option<String>)> = stmt
                .query_map(params![src], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);

            // Validate BEFORE touching destination — DELETE-then-filter wiped
            // dest when every source pattern was invalid (recurring review bug).
            let (replacement, skipped) = prepare_badword_replacement(rows)?;
            result.skipped_badwords = skipped;

            tx.execute("DELETE FROM badwords WHERE guild_id = ?1", params![dst])?;
            for (pattern, note) in &replacement {
                tx.execute(
                    "INSERT INTO badwords (guild_id, pattern, note) VALUES (?1, ?2, ?3)",
                    params![dst, pattern.clone(), note.clone()],
                )?;
            }
            tracing::debug!(
                "copy_guild_config badwords src={src} dst={dst}: inserted={} skipped={skipped}",
                replacement.len()
            );
            result.copied.push("badwords");
        }

        if wants("commands") {
            let mut stmt = tx.prepare("SELECT name, data FROM commands WHERE guild_id = ?1")?;
            let rows: Vec<(String, String)> = stmt
                .query_map(params![src], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            tx.execute("DELETE FROM commands WHERE guild_id = ?1", params![dst])?;
            for (name, data) in rows {
                tx.execute(
                    "INSERT INTO commands (guild_id, name, data) VALUES (?1, ?2, ?3)
                     ON CONFLICT(guild_id, name) DO UPDATE SET data = excluded.data",
                    params![dst, name, data],
                )?;
            }
            result.copied.push("commands");
        }

        if wants("templates") {
            let mut stmt =
                tx.prepare("SELECT event_type, template FROM templates WHERE guild_id = ?1")?;
            let rows: Vec<(String, String)> = stmt
                .query_map(params![src], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            tx.execute("DELETE FROM templates WHERE guild_id = ?1", params![dst])?;
            for (event, template) in rows {
                tx.execute(
                    "INSERT INTO templates (guild_id, event_type, template) VALUES (?1, ?2, ?3)
                     ON CONFLICT(guild_id, event_type) DO UPDATE SET template = excluded.template",
                    params![dst, event, template],
                )?;
            }
            result.copied.push("templates");
        }

        if wants("logchannels") {
            // Discord channel snowflakes are guild-scoped. Copying them to
            // another guild (or into/out of global) routes events to invalid
            // or foreign channels. Skip rather than silently misconfigure.
            if src == dst {
                tracing::debug!(
                    "copy_guild_config: no-op logchannels src=dst={src} \
                     (channels already belong to this server)"
                );
                // Do not report as copied — nothing changed.
            } else {
                tracing::warn!(
                    "copy_guild_config: skipping logchannels src={src} dst={dst} \
                     (channel IDs are not portable across Discord servers)"
                );
                result.skipped_logchannels = true;
                // Hard refuse already handled in preflight for logchannels-only.
            }
        }

        if !result.copied.is_empty() {
            bump_config_version_conn(&tx)?;
            if result.copied.contains(&"commands") {
                bump_slash_registry_version_conn(&tx)?;
            }
        }
        if copied_settings {
            refresh_settings_heal_checksum_conn(&tx)?;
        }
        tx.commit()?;
        Ok(result)
    })
    .await
}

// ---------------------------------------------------------------------------
// full guild config (all scopes resolved) — used by the in-memory config cache
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ConfigParts {
    pub settings: GuildSettings,
    pub rules: std::collections::HashMap<String, RuleDef>,
    pub commands: std::collections::HashMap<String, CommandDef>,
    pub templates: std::collections::HashMap<String, String>,
    pub log_channels: std::collections::HashMap<String, i64>,
    pub badwords: Vec<String>,
}

/// Loads `(key, value)` rows where the value column holds JSON-serialized
/// config (rules, commands). The value column name varies per table.
fn rows_map_json<T: DeserializeOwned + Send + 'static>(
    conn: &impl SqlExecutor,
    table: &str,
    key_col: &str,
    value_col: &str,
    guild_id: i64,
) -> Result<std::collections::HashMap<String, T>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {key_col}, {value_col} FROM {table} WHERE guild_id = ?1"
    ))?;
    let rows = stmt.query_map(params![guild_id], |r| {
        Ok((r.get::<String>(0)?, r.get::<String>(1)?))
    })?;
    let mut out = std::collections::HashMap::new();
    for row in rows {
        let (k, v) = row?;
        out.insert(k, read_json(&v, table)?);
    }
    Ok(out)
}

/// Loads `(key, value)` rows where the value column is plain text
/// (templates) or an integer (log_channels).
trait PlainValue: Sized {
    fn get(row: &Row, index: i32) -> Result<Self>;
}

impl PlainValue for String {
    fn get(row: &Row, index: i32) -> Result<Self> {
        Ok(row.get(index)?)
    }
}

impl PlainValue for i64 {
    fn get(row: &Row, index: i32) -> Result<Self> {
        Ok(row.get(index)?)
    }
}

fn rows_map_plain<T: PlainValue + Send + 'static>(
    conn: &impl SqlExecutor,
    table: &str,
    key_col: &str,
    value_col: &str,
    guild_id: i64,
) -> Result<std::collections::HashMap<String, T>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {key_col}, {value_col} FROM {table} WHERE guild_id = ?1"
    ))?;
    let rows = stmt.query_map(params![guild_id], |r| {
        Ok((r.get::<String>(0)?, T::get(r, 1)?))
    })?;
    let mut out = std::collections::HashMap::new();
    for row in rows {
        let (k, v) = row?;
        out.insert(k, v);
    }
    Ok(out)
}

pub async fn load_config_parts(db: &Db, guild_id: i64) -> Result<ConfigParts> {
    Ok(load_config_parts_with_version(db, guild_id).await?.1)
}

pub async fn load_config_parts_with_version(db: &Db, guild_id: i64) -> Result<(u64, ConfigParts)> {
    db.with_conn(move |conn| {
        let tx = conn.transaction()?;
        let version = get_config_version_conn(&tx)?;
        let conn = &tx;
        let defaults = terminal_defaults_conn(conn)?;
        // settings (effective: field-level guild → global → config terminal defaults)
        let g_settings = if guild_id == GLOBAL_SCOPE {
            None
        } else {
            read_settings_override_conn(conn, guild_id)?
        };
        let gl_settings = read_settings_override_conn(conn, GLOBAL_SCOPE)?;
        let settings = resolve_settings(&g_settings, &gl_settings, &defaults);

        // rules: map rule_id -> effective def
        let g_rules: std::collections::HashMap<String, RuleDef> =
            rows_map_json(conn, "rules", "rule_id", "data", guild_id)?;
        let gl_rules: std::collections::HashMap<String, RuleDef> =
            rows_map_json(conn, "rules", "rule_id", "data", GLOBAL_SCOPE)?;
        let default_rules = crate::defaults::default_rules();
        let mut rules = std::collections::HashMap::new();
        for (i, id) in crate::types::RULE_IDS.iter().enumerate() {
            let def = default_rules[i].clone();
            rules.insert(
                id.to_string(),
                resolve(
                    &g_rules.get(*id).cloned(),
                    &gl_rules.get(*id).cloned(),
                    &def,
                ),
            );
        }

        // commands: name(lowercase) -> effective def (guild ?? global)
        let g_cmds: std::collections::HashMap<String, CommandDef> =
            rows_map_json(conn, "commands", "name", "data", guild_id)?;
        let gl_cmds: std::collections::HashMap<String, CommandDef> =
            rows_map_json(conn, "commands", "name", "data", GLOBAL_SCOPE)?;
        let mut commands: std::collections::HashMap<String, CommandDef> = gl_cmds.clone();
        for (name, def) in g_cmds {
            commands.insert(name, def);
        }

        // templates: effective per event (plain text column `template`)
        let g_tpl: std::collections::HashMap<String, String> =
            rows_map_plain(conn, "templates", "event_type", "template", guild_id)?;
        let gl_tpl: std::collections::HashMap<String, String> =
            rows_map_plain(conn, "templates", "event_type", "template", GLOBAL_SCOPE)?;
        let mut templates = std::collections::HashMap::new();
        for (event, def) in crate::defaults::default_templates() {
            let resolved = resolve(
                &g_tpl.get(event).cloned(),
                &gl_tpl.get(event).cloned(),
                &def.to_string(),
            );
            templates.insert(event.to_string(), resolved);
        }
        for (event, t) in &g_tpl {
            templates.insert(event.clone(), t.clone());
        }
        for (event, t) in &gl_tpl {
            templates.entry(event.clone()).or_insert_with(|| t.clone());
        }

        // log channels: effective per event (integer column `channel_id`)
        let g_lc: std::collections::HashMap<String, i64> =
            rows_map_plain(conn, "log_channels", "event_type", "channel_id", guild_id)?;
        let mut log_channels = std::collections::HashMap::new();
        // Discord channel IDs belong to exactly one guild. Deliberately ignore
        // legacy global rows: falling back to one would post another guild's
        // moderation events into a foreign channel.
        for (event, c) in &g_lc {
            log_channels.insert(event.clone(), *c);
        }

        // badwords: effective = global + guild (union)
        let mut badwords = Vec::new();
        for scope in [GLOBAL_SCOPE, guild_id] {
            let mut stmt = conn.prepare("SELECT pattern FROM badwords WHERE guild_id = ?1")?;
            let rows = stmt.query_map(params![scope], |r| Ok(r.get::<String>(0)?))?;
            for row in rows {
                badwords.push(row?);
            }
        }

        let parts = ConfigParts {
            settings,
            rules,
            commands,
            templates,
            log_channels,
            badwords,
        };
        tx.finish_read()?;
        Ok((version, parts))
    })
    .await
}

/// Loads the effective command registry and its durable version from one
/// read transaction. This prevents a remote replica from supplying version N
/// on one pooled connection and stale N-1 command rows on another.
pub async fn load_slash_commands_with_version(
    db: &Db,
    guild_id: i64,
) -> Result<(u64, std::collections::HashMap<String, CommandDef>)> {
    db.with_conn(move |conn| {
        let tx = conn.transaction()?;
        let version = get_slash_registry_version_conn(&tx)?;
        let guild: std::collections::HashMap<String, CommandDef> =
            rows_map_json(&tx, "commands", "name", "data", guild_id)?;
        let mut commands: std::collections::HashMap<String, CommandDef> =
            rows_map_json(&tx, "commands", "name", "data", GLOBAL_SCOPE)?;
        commands.extend(guild);
        tx.finish_read()?;
        Ok((version, commands))
    })
    .await
}
