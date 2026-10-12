use super::*;
use serde::Deserialize;

/// Guild-local exclusions for one log event. Both lists contain canonical
/// decimal Discord snowflake IDs, serialized as strings for the WebUI.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogFilter {
    pub users: Vec<String>,
    pub channels: Vec<String>,
}

impl LogFilter {
    pub fn normalized(mut self) -> Result<Self> {
        anyhow::ensure!(
            self.users.len() <= 64 && self.channels.len() <= 64,
            "too many log exclusions"
        );
        self.users = self
            .users
            .into_iter()
            .map(|id| canonical_log_filter_id(&id, "user"))
            .collect::<Result<Vec<_>>>()?;
        self.users.sort();
        self.users.dedup();
        self.channels = self
            .channels
            .into_iter()
            .map(|id| canonical_log_filter_id(&id, "source channel"))
            .collect::<Result<Vec<_>>>()?;
        self.channels.sort();
        self.channels.dedup();
        Ok(self)
    }
}

fn canonical_log_filter_id(raw: &str, kind: &str) -> Result<String> {
    let id = raw
        .trim()
        .parse::<u64>()
        .with_context(|| format!("invalid ignored {kind} ID"))?;
    anyhow::ensure!(id > 0, "ignored {kind} ID must be positive");
    Ok(id.to_string())
}

pub async fn put_enrolled_log_filter(
    db: &Db,
    scope: ConfigurationWriteScope,
    event: &str,
    filter: LogFilter,
) -> Result<LogFilter> {
    anyhow::ensure!(crate::types::is_log_event(event), "unknown log event type");
    anyhow::ensure!(scope.guild_id() > 0, "log filters are guild-only");
    let filter = filter.normalized()?;
    let keys = vec![scope.guild_id().into(), event.to_owned().into()];
    if filter.users.is_empty() && filter.channels.is_empty() {
        delete_row(
            db,
            "log_filters",
            "guild_id, event_type",
            keys,
            false,
            Some(scope),
        )
        .await?;
    } else {
        upsert_row(
            db,
            "log_filters",
            "guild_id, event_type",
            keys,
            filter.clone(),
            false,
            Some(scope),
        )
        .await?;
    }
    Ok(filter)
}

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
pub fn resolve<T: Clone>(guild_row: &Option<T>, global_row: &Option<T>, default: &T) -> T {
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
    let key_value = key_value.map(str::to_owned);
    db.with_conn(move |conn| load_rows_conn(conn, guild_id, table, key_col, key_value.as_deref()))
        .await
}

fn load_rows_conn<T: DeserializeOwned>(
    conn: &impl SqlExecutor,
    guild_id: i64,
    table: &'static str,
    key_col: &'static str,
    key_value: Option<&str>,
) -> Result<Vec<(String, T)>> {
    let sql = if key_value.is_some() {
        format!("SELECT {key_col}, data FROM {table} WHERE guild_id = ?1 AND {key_col} = ?2")
    } else {
        format!("SELECT {key_col}, data FROM {table} WHERE guild_id = ?1")
    };
    let mut stmt = conn.prepare(&sql)?;
    let mut args: Vec<Value> = vec![guild_id.into()];
    if let Some(kv) = key_value {
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
}

async fn upsert_row<T: Serialize + Send + 'static>(
    db: &Db,
    table: &'static str,
    key_cols: &'static str,
    key_params: Vec<Value>,
    data: T,
    bump_slash_registry: bool,
    expected: Option<ConfigurationWriteScope>,
) -> Result<()> {
    let json = serde_json::to_string(&data)?;
    let guild_id = match key_params.first() {
        Some(Value::Integer(guild_id)) => *guild_id,
        _ => GLOBAL_SCOPE,
    };
    db.with_conn(move |conn| {
        let keys = key_params
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let n = key_params.len() + 1;
        let tx = conn.transaction_immediate()?;
        verify_configuration_write(&tx, expected.as_ref(), guild_id)?;
        tx.execute_rows(
            &format!(
                "INSERT INTO {table} ({key_cols}, data) VALUES ({keys}, ?{n})
                 ON CONFLICT({key_cols}) DO UPDATE SET data = excluded.data"
            ),
            params_from_iter(key_params.into_iter().chain(std::iter::once(json.into()))),
            1,
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
    expected: Option<ConfigurationWriteScope>,
) -> Result<()> {
    let guild_id = match key_params.first() {
        Some(Value::Integer(guild_id)) => *guild_id,
        _ => GLOBAL_SCOPE,
    };
    db.with_conn(move |conn| {
        let keys = key_params
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let mut tx = conn.transaction_primary_read()?;
        verify_configuration_write(&tx, expected.as_ref(), guild_id)?;
        let exists: bool = tx.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE ({key_cols}) = ({keys}))"),
            params_from_iter(key_params.clone()),
            |r| Ok(r.get(0)?),
        )?;
        if !exists {
            tx.finish_read()?;
            return Ok(());
        }
        tx.prepare_reclaim()?;
        let n = tx.execute_reclaim(
            &format!("DELETE FROM {table} WHERE ({key_cols}) = ({keys})"),
            params_from_iter(key_params),
        )?;
        // No-op deletes must not bump versions — that forced cross-process
        // cache invalidation and slash re-sync sweeps for already-gone rows.
        if n > 0 {
            bump_config_version_reclaim_conn(&tx)?;
            if bump_slash_registry {
                bump_slash_version_reclaim_conn(&tx)?;
            }
        }
        tx.commit()?;
        Ok(())
    })
    .await
}

fn verify_configuration_write(
    conn: &impl SqlExecutor,
    expected: Option<&ConfigurationWriteScope>,
    guild_id: i64,
) -> Result<()> {
    if let Some(scope) = expected {
        scope.verify_conn(conn)
    } else if guild_id > 0 {
        super::guild_stores::refuse_fenced_write(conn, guild_id)
    } else {
        Ok(())
    }
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
    crate::image_spam::validate_rule(rule_id, rule)?;
    upsert_row(
        db,
        "rules",
        "guild_id, rule_id",
        vec![guild_id.into(), rule_id.to_string().into()],
        rule.clone(),
        false,
        None,
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
        None,
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
        cmd.clone(),
        true,
        None,
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
        None,
    )
    .await
}

// ---------------------------------------------------------------------------
// templates
// ---------------------------------------------------------------------------

pub async fn list_templates(db: &Db, guild_id: i64) -> Result<Vec<(String, String)>> {
    db.with_conn(move |conn| list_templates_conn(conn, guild_id))
        .await
}

fn list_templates_conn(conn: &impl SqlExecutor, guild_id: i64) -> Result<Vec<(String, String)>> {
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
}

pub async fn upsert_template(db: &Db, guild_id: i64, event: &str, template: &str) -> Result<()> {
    upsert_template_inner(db, guild_id, event, template, None).await
}

async fn upsert_template_inner(
    db: &Db,
    guild_id: i64,
    event: &str,
    template: &str,
    expected: Option<ConfigurationWriteScope>,
) -> Result<()> {
    let (event, template) = (event.to_string(), template.to_string());
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        verify_configuration_write(&tx, expected.as_ref(), guild_id)?;
        tx.execute_rows(
            "INSERT INTO templates (guild_id, event_type, template) VALUES (?1, ?2, ?3)
             ON CONFLICT(guild_id, event_type) DO UPDATE SET template = excluded.template",
            params![guild_id, event, template],
            1,
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
        None,
    )
    .await
}

pub async fn get_template_row(db: &Db, guild_id: i64, event: &str) -> Result<Option<String>> {
    let event = event.to_owned();
    db.with_conn(move |conn| get_template_row_conn(conn, guild_id, &event))
        .await
}

fn get_template_row_conn(
    conn: &impl SqlExecutor,
    guild_id: i64,
    event: &str,
) -> Result<Option<String>> {
    conn.query_row(
        "SELECT template FROM templates WHERE guild_id = ?1 AND event_type = ?2",
        params![guild_id, event],
        |r| Ok(r.get(0)?),
    )
    .optional()
}

// ---------------------------------------------------------------------------
// log channels
// ---------------------------------------------------------------------------

pub async fn list_log_channels(db: &Db, guild_id: i64) -> Result<Vec<(String, i64)>> {
    db.with_conn(move |conn| list_log_channels_conn(conn, guild_id))
        .await
}

fn list_log_channels_conn(conn: &impl SqlExecutor, guild_id: i64) -> Result<Vec<(String, i64)>> {
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
}

/// Shared mapping/version semantics for direct writes and queued completion.
/// The caller owns the transaction and any queue state or supersession update.
pub(super) fn write_log_channel_conn(
    conn: &impl SqlExecutor,
    guild_id: i64,
    event: &str,
    channel_id: Option<i64>,
) -> Result<()> {
    let changed = if let Some(channel_id) = channel_id {
        conn.execute_rows(
            "INSERT INTO log_channels (guild_id, event_type, channel_id) VALUES (?1, ?2, ?3)
             ON CONFLICT(guild_id, event_type) DO UPDATE SET channel_id = excluded.channel_id",
            params![guild_id, event, channel_id],
            1,
        )?
    } else {
        conn.execute_reclaim(
            "DELETE FROM log_channels WHERE guild_id=?1 AND event_type=?2",
            params![guild_id, event],
        )?
    };
    if changed > 0 {
        bump_config_version_conn(conn)?;
    }
    Ok(())
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
        super::guild_stores::refuse_fenced_write(&tx, guild_id)?;
        write_log_channel_conn(&tx, guild_id, &event, Some(channel_id))?;
        supersede_log_action(&tx, guild_id, &event)?;
        tx.commit()?;
        Ok(())
    })
    .await
}

pub async fn delete_log_channel(db: &Db, guild_id: i64, event: &str) -> Result<()> {
    let event = event.to_owned();
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        super::guild_stores::refuse_fenced_write(&tx, guild_id)?;
        write_log_channel_conn(&tx, guild_id, &event, None)?;
        // A clear supersedes queued intent even when the mapping is absent.
        supersede_log_action(&tx, guild_id, &event)?;
        tx.commit()?;
        Ok(())
    })
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
    db.with_conn(move |conn| list_badwords_conn(conn, guild_id))
        .await
}

fn list_badwords_conn(conn: &impl SqlExecutor, guild_id: i64) -> Result<Vec<Badword>> {
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
}

pub async fn add_badword(db: &Db, guild_id: i64, pattern: &str, note: Option<&str>) -> Result<i64> {
    add_badword_inner(db, guild_id, pattern, note, None).await
}

async fn add_badword_inner(
    db: &Db,
    guild_id: i64,
    pattern: &str,
    note: Option<&str>,
    expected: Option<ConfigurationWriteScope>,
) -> Result<i64> {
    // Single gate for every writer (API, future commands) — reject before INSERT.
    crate::normalize::compile_user_regex(pattern).map_err(|e| anyhow::anyhow!(e))?;
    let (pattern, note) = (pattern.to_string(), note.map(|s| s.to_string()));
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        verify_configuration_write(&tx, expected.as_ref(), guild_id)?;
        tx.execute_rows(
            "INSERT INTO badwords (guild_id, pattern, note) VALUES (?1, ?2, ?3)",
            params![guild_id, pattern, note],
            1,
        )?;
        let id = tx.last_insert_rowid();
        bump_config_version_conn(&tx)?;
        tx.commit()?;
        Ok(id)
    })
    .await
}

/// Authoritative lookup used to authorize a following mutation. Ordinary
/// replica reads can legitimately lag a just-committed add.
pub async fn get_badword_primary(db: &Db, id: i64) -> Result<Option<Badword>> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let word = tx
            .query_row(
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
            .optional()?;
        tx.finish_read()?;
        Ok(word)
    })
    .await
}

/// Deletes a badword only when `id` belongs to the exact `guild_id` scope.
/// Callers that want to delete a global row must pass `GLOBAL_SCOPE` and
/// enforce owner auth at the API layer.
pub async fn delete_badword(db: &Db, id: i64, guild_id: i64) -> Result<()> {
    delete_badword_inner(db, id, guild_id, None).await
}

async fn delete_badword_inner(
    db: &Db,
    id: i64,
    guild_id: i64,
    expected: Option<ConfigurationWriteScope>,
) -> Result<()> {
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        verify_configuration_write(&tx, expected.as_ref(), guild_id)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM badwords WHERE id=?1 AND guild_id=?2)",
            params![id, guild_id],
            |r| Ok(r.get(0)?),
        )?;
        if !exists {
            tx.finish_read()?;
            anyhow::bail!("badword not found");
        }
        tx.prepare_reclaim()?;
        let n = tx.execute_reclaim(
            "DELETE FROM badwords WHERE id = ?1 AND guild_id = ?2",
            params![id, guild_id],
        )?;
        if n == 0 {
            // Same public wording as API denials — never leak guild/scope detail.
            anyhow::bail!("badword not found");
        }
        bump_config_version_reclaim_conn(&tx)?;
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

/// Validates a rules copy before the destination is cleared.
///
/// Custom rows use the same create admission as an upsert: global scope is
/// rejected, the per-guild cap applies to the replacement set, and each new
/// custom rule is stored dry-run.
fn prepare_copied_rules(dst: i64, rows: Vec<(String, RuleDef)>) -> Result<Vec<(String, RuleDef)>> {
    let mut replacement = Vec::with_capacity(rows.len());
    let mut custom = 0usize;
    for (id, rule) in rows {
        crate::image_spam::validate_rule(&id, &rule)?;
        if crate::custom_rules::is_custom_id(&id) {
            custom += 1;
            replacement.push((
                id.clone(),
                crate::custom_rules::custom_rule_for_create(dst, &id, rule)?,
            ));
        } else {
            replacement.push((id, rule));
        }
    }
    crate::custom_rules::ensure_room_for_new_custom_rules(0, custom)?;
    Ok(replacement)
}

// The caller keeps validation, deletion and these budgeted inserts in one transaction.
pub(super) fn insert_copied_rules(
    dst: i64,
    replacement: &[(String, RuleDef)],
    mut execute: impl FnMut(&str, Vec<Value>, u64) -> Result<usize>,
) -> Result<()> {
    // Match the badword-copy batch size to bound parameters and remote requests.
    for chunk in replacement.chunks(100) {
        let values = (0..chunk.len())
            .map(|i| {
                let n = i * 3 + 1;
                format!("(?{n}, ?{}, ?{})", n + 1, n + 2)
            })
            .collect::<Vec<_>>()
            .join(", ");
        let mut args = Vec::with_capacity(chunk.len() * 3);
        for (id, rule) in chunk {
            args.push(Value::Integer(dst));
            args.push(Value::Text(id.clone()));
            args.push(Value::Text(serde_json::to_string(rule)?));
        }
        execute(
            &format!("INSERT INTO rules (guild_id, rule_id, data) VALUES {values}"),
            args,
            chunk.len() as u64,
        )?;
    }
    Ok(())
}

fn section_wanted(sections: &[String], name: &str) -> bool {
    sections.iter().any(|s| s == name || s == "all")
}

/// Validate structural copy refusal conditions before changing rows. Any
/// data-dependent validation stays inside the same primary write snapshot.
fn validate_copy_sections(src: i64, dst: i64, sections: &[String]) -> Result<()> {
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

/// Copies selected config sections from `src` to `dst` in one transaction
/// with a single `config_version` bump (avoids mid-copy cache snapshots).
///
/// Validates refusal conditions inside the primary write snapshot so replica
/// lag cannot falsely reject a copy and a concurrent source change cannot skip
/// validation.
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
    copy_guild_config_inner(db, src, dst, sections, None).await
}

/// Both scopes are checked inside the same primary write transaction as the copy.
pub async fn copy_enrolled_guild_config(
    db: &Db,
    source: ConfigurationScope,
    destination: ConfigurationScope,
    sections: Vec<String>,
) -> Result<CopyResult> {
    copy_guild_config_inner(
        db,
        source.guild_id(),
        destination.guild_id(),
        sections,
        Some((source, destination)),
    )
    .await
}

async fn copy_guild_config_inner(
    db: &Db,
    src: i64,
    dst: i64,
    sections: Vec<String>,
    expected: Option<(ConfigurationScope, ConfigurationScope)>,
) -> Result<CopyResult> {
    let write_guard = Arc::clone(&db.settings_write_lock).lock_owned().await;
    db.with_conn(move |conn| {
        let _write_guard = write_guard;
        let tx = conn.transaction_immediate()?;
        if let Some((source, destination)) = &expected {
            source.verify_conn(&tx)?;
            destination.verify_conn(&tx)?;
        }
        verify_configuration_write(&tx, None, dst)?;

        let defaults = terminal_defaults_conn(&tx)?;
        // Always re-validate in the same primary snapshot as the write.
        validate_copy_sections(src, dst, &sections)?;
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
            let rows = load_rows_conn::<RuleDef>(&tx, src, "rules", "rule_id", None)?;
            if src != dst {
                let replacement = prepare_copied_rules(dst, rows)?;
                tx.execute_reclaim("DELETE FROM rules WHERE guild_id = ?1", params![dst])?;
                insert_copied_rules(dst, &replacement, |sql, args, rows| {
                    tx.execute_insert_batch(sql, params_from_iter(args), rows)
                })?;
            } else {
                for (id, rule) in &rows {
                    crate::image_spam::validate_rule(id, rule)?;
                }
            }
            result.copied.push("rules");
        }

        if wants("badwords") {
            const MAX_COPY_BADWORDS: usize = 1_000;
            let mut stmt =
                tx.prepare("SELECT pattern, note FROM badwords WHERE guild_id = ?1 LIMIT 1001")?;
            let rows: Vec<(String, Option<String>)> = stmt
                .query_map(params![src], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            if rows.len() > MAX_COPY_BADWORDS {
                anyhow::bail!("badword copy is limited to {MAX_COPY_BADWORDS} rows per operation");
            }

            // Validate BEFORE touching destination — DELETE-then-filter wiped
            // dest when every source pattern was invalid (recurring review bug).
            let (replacement, skipped) = prepare_badword_replacement(rows)?;
            result.skipped_badwords = skipped;

            if src != dst {
                tx.execute_reclaim("DELETE FROM badwords WHERE guild_id = ?1", params![dst])?;
                // Keep remote round trips bounded: validated rows are inserted
                // in parameterized batches rather than one request per row.
                for chunk in replacement.chunks(100) {
                    let values = (0..chunk.len())
                        .map(|i| {
                            let n = i * 3 + 1;
                            format!("(?{n}, ?{}, ?{})", n + 1, n + 2)
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    let mut args = Vec::with_capacity(chunk.len() * 3);
                    for (pattern, note) in chunk {
                        args.push(Value::Integer(dst));
                        args.push(Value::Text(pattern.clone()));
                        args.push(note.clone().map(Value::Text).unwrap_or(Value::Null));
                    }
                    tx.execute_rows(
                        &format!("INSERT INTO badwords (guild_id, pattern, note) VALUES {values}"),
                        params_from_iter(args),
                        chunk.len() as u64,
                    )?;
                }
            }
            tracing::debug!(
                "copy_guild_config badwords src={src} dst={dst}: inserted={} skipped={skipped}",
                replacement.len()
            );
            result.copied.push("badwords");
        }

        if wants("commands") {
            if src != dst {
                tx.execute_reclaim("DELETE FROM commands WHERE guild_id = ?1", params![dst])?;
                tx.execute_copy(
                    "INSERT INTO commands (guild_id, name, data)
                     SELECT ?1, name, data FROM commands WHERE guild_id = ?2",
                    dst,
                    src,
                    "commands",
                    "name",
                    "data",
                )?;
            }
            result.copied.push("commands");
        }

        if wants("templates") {
            if src != dst {
                tx.execute_reclaim("DELETE FROM templates WHERE guild_id = ?1", params![dst])?;
                tx.execute_copy(
                    "INSERT INTO templates (guild_id, event_type, template)
                     SELECT ?1, event_type, template FROM templates WHERE guild_id = ?2",
                    dst,
                    src,
                    "templates",
                    "event_type",
                    "template",
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
            invalidate_settings_heal_checksum_conn(&tx)?;
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
    pub log_filters: std::collections::HashMap<String, LogFilter>,
    pub badwords: Vec<String>,
    pub known_bad_images: Vec<KnownBadImage>,
}

/// Raw and effective data needed by one WebUI config response. Every field is
/// read in the same versioned transaction so the UI cannot combine replica
/// snapshots from different points in time.
pub struct WebConfigSnapshot {
    pub version: u64,
    pub settings: SettingsWebBundle,
    pub nickname_reconcile: Option<NicknameReconcileStatus>,
    pub guild_rules: std::collections::HashMap<String, RuleDef>,
    pub global_rules: std::collections::HashMap<String, RuleDef>,
    pub badwords: Vec<Badword>,
    pub known_bad_images: Vec<KnownBadImage>,
    pub guild_commands: std::collections::HashMap<String, CommandDef>,
    pub global_commands: std::collections::HashMap<String, CommandDef>,
    pub guild_templates: std::collections::HashMap<String, String>,
    pub global_templates: std::collections::HashMap<String, String>,
    pub guild_log_channels: std::collections::HashMap<String, i64>,
    pub guild_log_filters: std::collections::HashMap<String, LogFilter>,
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

/// One Hrana statement for every configuration collection in a snapshot.
/// Bunny Database / Turso closes interactive transactions after a few seconds;
/// the previous per-table prepare+query loop exceeded that on Magic Containers.
const CONFIGURATION_SNAPSHOT_SQL: &str = r#"
SELECT
  (SELECT value FROM meta WHERE key = 'config_version'),
  (SELECT value FROM meta WHERE key = 'slash_registry_version'),
  (SELECT value FROM meta WHERE key = 'config_revision'),
  (SELECT value FROM meta WHERE key = 'slash_registry_revision'),
  (SELECT value FROM meta WHERE key = 'settings_terminal_defaults'),
  (SELECT settings FROM guild_settings WHERE guild_id = ?2),
  (SELECT settings FROM guild_settings WHERE guild_id = ?1),
  (SELECT json_group_object(rule_id, data) FROM rules WHERE guild_id = ?1),
  (SELECT json_group_object(rule_id, data) FROM rules WHERE guild_id = ?2),
  (SELECT json_group_object(name, data) FROM commands WHERE guild_id = ?1),
  (SELECT json_group_object(name, data) FROM commands WHERE guild_id = ?2),
  (SELECT json_group_object(event_type, template) FROM templates WHERE guild_id = ?1),
  (SELECT json_group_object(event_type, template) FROM templates WHERE guild_id = ?2),
  (SELECT json_group_object(event_type, channel_id) FROM log_channels WHERE guild_id = ?1),
  (SELECT json_group_array(json_object(
        'id', id,
        'guild_id', guild_id,
        'pattern', pattern,
        'note', note
     ))
     FROM (SELECT id, guild_id, pattern, note FROM badwords
           WHERE guild_id = ?1 OR guild_id = ?2 ORDER BY id)),
  (SELECT state FROM nickname_reconcile WHERE guild_id = ?1),
  (SELECT attempt_count FROM nickname_reconcile WHERE guild_id = ?1),
  (SELECT next_attempt_at FROM nickname_reconcile WHERE guild_id = ?1),
  (SELECT last_error_code FROM nickname_reconcile WHERE guild_id = ?1),
  (SELECT last_error FROM nickname_reconcile WHERE guild_id = ?1),
  (SELECT last_error_at FROM nickname_reconcile WHERE guild_id = ?1)
  ,(SELECT json_group_object(event_type, data) FROM log_filters WHERE guild_id = ?1)
  ,(SELECT json_group_array(json_object('hash', hash, 'added_at', added_at,
        'added_via', added_via, 'actor', actor, 'note', note))
     FROM (SELECT hash, added_at, added_via, actor, note FROM known_bad_images
           WHERE guild_id = ?1 ORDER BY hash LIMIT 1024))
"#;

struct ConfigurationSnapshotRows {
    versions: VersionState,
    defaults: GuildSettings,
    guild_settings_raw: Option<String>,
    global_settings_raw: Option<String>,
    guild_rules: std::collections::HashMap<String, RuleDef>,
    global_rules: std::collections::HashMap<String, RuleDef>,
    guild_commands: std::collections::HashMap<String, CommandDef>,
    global_commands: std::collections::HashMap<String, CommandDef>,
    guild_templates: std::collections::HashMap<String, String>,
    global_templates: std::collections::HashMap<String, String>,
    guild_log_channels: std::collections::HashMap<String, i64>,
    guild_log_filters: std::collections::HashMap<String, LogFilter>,
    badwords: Vec<Badword>,
    known_bad_images: Vec<KnownBadImage>,
    nickname_reconcile: Option<NicknameReconcileStatus>,
}

fn parse_meta_version(raw: Option<String>) -> u64 {
    raw.as_deref().unwrap_or("0").parse::<u64>().unwrap_or(0)
}

fn json_agg_text(raw: Option<String>, empty: &str) -> String {
    match raw {
        Some(value) if !value.is_empty() && value != "null" => value,
        _ => empty.to_owned(),
    }
}

fn parse_json_encoded_map<T: DeserializeOwned>(
    raw: Option<String>,
    what: &str,
) -> Result<std::collections::HashMap<String, T>> {
    let encoded: std::collections::HashMap<String, String> =
        serde_json::from_str(&json_agg_text(raw, "{}"))
            .with_context(|| format!("corrupt JSON object in {what}"))?;
    let mut out = std::collections::HashMap::new();
    for (key, value) in encoded {
        out.insert(key, read_json(&value, what)?);
    }
    Ok(out)
}

fn parse_json_map<T: DeserializeOwned>(
    raw: Option<String>,
    what: &str,
) -> Result<std::collections::HashMap<String, T>> {
    serde_json::from_str(&json_agg_text(raw, "{}"))
        .with_context(|| format!("corrupt JSON object in {what}"))
}

fn parse_json_vec<T: DeserializeOwned>(raw: Option<String>, what: &str) -> Result<Vec<T>> {
    serde_json::from_str(&json_agg_text(raw, "[]"))
        .with_context(|| format!("corrupt JSON array in {what}"))
}

fn settings_override_from_raw(guild_id: i64, raw: Option<String>) -> Option<GuildSettingsOverride> {
    raw.as_deref()
        .and_then(|json| parse_settings_override_json(guild_id, json))
}

fn load_configuration_snapshot_conn(
    conn: &impl SqlExecutor,
    guild_id: i64,
) -> Result<ConfigurationSnapshotRows> {
    conn.query_row(
        CONFIGURATION_SNAPSHOT_SQL,
        params![guild_id, GLOBAL_SCOPE],
        |row| {
            let config = parse_meta_version(row.get(0)?);
            let slash = parse_meta_version(row.get(1)?);
            let config_revision = row
                .get::<Option<String>>(2)?
                .unwrap_or_else(|| format!("legacy:{config}"));
            let slash_revision = row
                .get::<Option<String>>(3)?
                .unwrap_or_else(|| format!("legacy:{slash}"));
            let defaults_raw: Option<String> = row.get(4)?;
            let defaults =
                terminal_defaults_from_json(defaults_raw.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "meta.settings_terminal_defaults is missing; start jmodguard-bot first"
                    )
                })?)?;
            let global_settings_raw: Option<String> = row.get(5)?;
            let guild_settings_raw: Option<String> = row.get(6)?;
            let nick_state: Option<String> = row.get(15)?;
            let nickname_reconcile = match nick_state {
                Some(state) if guild_id != GLOBAL_SCOPE => Some(NicknameReconcileStatus {
                    state,
                    attempt_count: row.get::<Option<i64>>(16)?.unwrap_or(0) as u32,
                    next_attempt_at: row.get::<Option<i64>>(17)?.unwrap_or(0),
                    last_error_code: row.get(18)?,
                    last_error: row.get(19)?,
                    last_error_at: row.get(20)?,
                }),
                _ => None,
            };
            Ok(ConfigurationSnapshotRows {
                versions: VersionState {
                    config,
                    slash,
                    config_revision,
                    slash_revision,
                },
                defaults,
                guild_settings_raw,
                global_settings_raw,
                guild_rules: parse_json_encoded_map(row.get(7)?, "rules")?,
                global_rules: parse_json_encoded_map(row.get(8)?, "rules")?,
                guild_commands: parse_json_encoded_map(row.get(9)?, "commands")?,
                global_commands: parse_json_encoded_map(row.get(10)?, "commands")?,
                guild_templates: parse_json_map(row.get(11)?, "templates")?,
                global_templates: parse_json_map(row.get(12)?, "templates")?,
                guild_log_channels: parse_json_map(row.get(13)?, "log_channels")?,
                guild_log_filters: parse_json_encoded_map(row.get(21)?, "log_filters")?,
                badwords: parse_json_vec(row.get(14)?, "badwords")?,
                known_bad_images: safe_known_bad_rows(parse_json_vec(
                    row.get(22)?,
                    "known_bad_images",
                )?),
                nickname_reconcile,
            })
        },
    )
}

impl ConfigurationSnapshotRows {
    fn settings_bundle(&self, guild_id: i64) -> SettingsWebBundle {
        let global_override =
            settings_override_from_raw(GLOBAL_SCOPE, self.global_settings_raw.clone());
        let global_settings = resolve_settings(&None, &global_override, &self.defaults);
        let (settings_override, settings) = if guild_id == GLOBAL_SCOPE {
            (global_override.clone(), global_settings.clone())
        } else {
            let guild_override =
                settings_override_from_raw(guild_id, self.guild_settings_raw.clone());
            let settings = resolve_settings(&guild_override, &global_override, &self.defaults);
            (guild_override, settings)
        };
        SettingsWebBundle {
            settings,
            settings_override,
            global_settings,
            global_settings_override: global_override,
        }
    }

    fn into_web_snapshot(self, guild_id: i64) -> WebConfigSnapshot {
        let settings = self.settings_bundle(guild_id);
        WebConfigSnapshot {
            version: self.versions.config,
            settings,
            nickname_reconcile: self.nickname_reconcile,
            guild_rules: self.guild_rules,
            global_rules: self.global_rules,
            badwords: self.badwords,
            known_bad_images: self.known_bad_images,
            guild_commands: self.guild_commands,
            global_commands: self.global_commands,
            guild_templates: self.guild_templates,
            global_templates: self.global_templates,
            guild_log_channels: self.guild_log_channels,
            guild_log_filters: self.guild_log_filters,
        }
    }

    fn into_config_parts(self, guild_id: i64) -> (u64, String, ConfigParts) {
        let g_settings = if guild_id == GLOBAL_SCOPE {
            None
        } else {
            settings_override_from_raw(guild_id, self.guild_settings_raw.clone())
        };
        let gl_settings =
            settings_override_from_raw(GLOBAL_SCOPE, self.global_settings_raw.clone());
        let settings = resolve_settings(&g_settings, &gl_settings, &self.defaults);
        let default_rules = crate::defaults::default_rules();
        let mut rules = std::collections::HashMap::new();
        for (i, id) in crate::types::RULE_IDS.iter().enumerate() {
            let def = default_rules[i].clone();
            rules.insert(
                id.to_string(),
                resolve(
                    &self.guild_rules.get(*id).cloned(),
                    &self.global_rules.get(*id).cloned(),
                    &def,
                ),
            );
        }
        if guild_id != GLOBAL_SCOPE {
            for (id, row) in &self.guild_rules {
                if !crate::custom_rules::is_custom_id(id) {
                    continue;
                }
                if crate::custom_rules::validate_custom_rule_shape(guild_id, id, row).is_ok() {
                    rules.insert(id.clone(), row.clone());
                }
            }
        }
        let mut commands = self.global_commands.clone();
        for (name, def) in self.guild_commands {
            commands.insert(name, def);
        }
        let mut templates = std::collections::HashMap::new();
        for (event, def) in crate::defaults::default_templates() {
            let resolved = resolve(
                &self.guild_templates.get(event).cloned(),
                &self.global_templates.get(event).cloned(),
                &def.to_string(),
            );
            templates.insert(event.to_string(), resolved);
        }
        for (event, t) in &self.guild_templates {
            templates.insert(event.clone(), t.clone());
        }
        for (event, t) in &self.global_templates {
            templates.entry(event.clone()).or_insert_with(|| t.clone());
        }
        let mut log_channels = std::collections::HashMap::new();
        // Discord channel IDs belong to exactly one guild. Deliberately ignore
        // legacy global rows: falling back to one would post another guild's
        // moderation events into a foreign channel.
        for (event, c) in &self.guild_log_channels {
            log_channels.insert(event.clone(), *c);
        }
        let mut badwords = Vec::new();
        for word in &self.badwords {
            if word.guild_id == GLOBAL_SCOPE {
                badwords.push(word.pattern.clone());
            }
        }
        for word in &self.badwords {
            if word.guild_id == guild_id && guild_id != GLOBAL_SCOPE {
                badwords.push(word.pattern.clone());
            }
        }
        (
            self.versions.config,
            self.versions.config_revision,
            ConfigParts {
                settings,
                rules,
                commands,
                templates,
                log_channels,
                log_filters: self.guild_log_filters,
                badwords,
                known_bad_images: self.known_bad_images,
            },
        )
    }
}

pub async fn load_config_parts(db: &Db, guild_id: i64) -> Result<ConfigParts> {
    Ok(load_config_parts_with_version(db, guild_id).await?.2)
}

async fn load_web_config_snapshot_primary(
    db: &Db,
    guild_id: i64,
    expected: Option<ConfigurationScope>,
) -> Result<WebConfigSnapshot> {
    retry_remote_interactive_read(|| {
        let db = db.clone();
        let expected = expected.clone();
        async move {
            db.with_conn(move |conn| {
                let tx = conn.transaction_primary_snapshot()?;
                if let Some(scope) = &expected {
                    scope.verify_conn(&tx)?;
                }
                let snapshot = load_configuration_snapshot_conn(&tx, guild_id)?;
                tx.finish_read()?;
                Ok(snapshot.into_web_snapshot(guild_id))
            })
            .await
        }
    })
    .await
}

/// Loads one coherent, primary-authoritative WebUI bundle. Nickname job state
/// has its own lifecycle rather than `config_version`, so a replica snapshot
/// cannot establish freshness for every operator-visible field.
pub async fn load_web_config_snapshot(db: &Db, guild_id: i64) -> Result<WebConfigSnapshot> {
    let snapshot = load_web_config_snapshot_primary(db, guild_id, None).await?;
    anyhow::ensure!(
        snapshot.version >= db.required_config_version(),
        "primary WebUI config snapshot is below the process's committed-version floor"
    );
    Ok(snapshot)
}

async fn load_config_parts_with_version_routed(
    db: &Db,
    guild_id: i64,
    primary: bool,
) -> Result<(u64, String, ConfigParts)> {
    retry_remote_interactive_read(|| {
        let db = db.clone();
        async move {
            db.with_conn(move |conn| {
                let tx = if primary {
                    conn.transaction_primary_snapshot()?
                } else {
                    conn.transaction_consistent_read()?
                };
                let snapshot = load_configuration_snapshot_conn(&tx, guild_id)?;
                tx.finish_read()?;
                Ok(snapshot.into_config_parts(guild_id))
            })
            .await
        }
    })
    .await
}

pub async fn load_config_parts_with_version(
    db: &Db,
    guild_id: i64,
) -> Result<(u64, String, ConfigParts)> {
    #[cfg(debug_assertions)]
    if let Some(snapshot) = db.take_test_config_snapshot() {
        return Ok(snapshot);
    }
    load_config_parts_with_version_routed(db, guild_id, false).await
}

/// Loads one internally consistent snapshot from the primary. Config-cache
/// callers use this only after an ordinary replica snapshot is below their
/// committed-version floor, so normal reads retain replica scalability.
pub async fn load_config_parts_with_version_primary(
    db: &Db,
    guild_id: i64,
) -> Result<(u64, String, ConfigParts)> {
    load_config_parts_with_version_routed(db, guild_id, true).await
}

/// Loads the effective command registry and its durable version from one
/// read transaction. This prevents a remote replica from supplying version N
/// on one pooled connection and stale N-1 command rows on another.
pub async fn load_slash_commands_with_version(
    db: &Db,
    guild_id: i64,
) -> Result<(u64, String, std::collections::HashMap<String, CommandDef>)> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_consistent_read()?;
        let versions = get_version_state_conn(&tx)?;
        let guild: std::collections::HashMap<String, CommandDef> =
            rows_map_json(&tx, "commands", "name", "data", guild_id)?;
        let mut commands: std::collections::HashMap<String, CommandDef> =
            rows_map_json(&tx, "commands", "name", "data", GLOBAL_SCOPE)?;
        commands.extend(guild);
        tx.finish_read()?;
        Ok((versions.slash, versions.slash_revision, commands))
    })
    .await
}

pub async fn load_slash_commands_with_version_primary(
    db: &Db,
    guild_id: i64,
) -> Result<(u64, String, std::collections::HashMap<String, CommandDef>)> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let versions = get_version_state_conn(&tx)?;
        let guild: std::collections::HashMap<String, CommandDef> =
            rows_map_json(&tx, "commands", "name", "data", guild_id)?;
        let mut commands: std::collections::HashMap<String, CommandDef> =
            rows_map_json(&tx, "commands", "name", "data", GLOBAL_SCOPE)?;
        commands.extend(guild);
        tx.finish_read()?;
        Ok((versions.slash, versions.slash_revision, commands))
    })
    .await
}

// Captured-scope entry points for future dashboard integration. Check inside each mutation.
pub async fn upsert_enrolled_rule(
    db: &Db,
    scope: ConfigurationWriteScope,
    id: &str,
    rule: &RuleDef,
) -> Result<()> {
    crate::image_spam::validate_rule(id, rule)?;
    if crate::custom_rules::is_custom_id(id) {
        return upsert_enrolled_custom_rule(db, scope, id, rule).await;
    }
    upsert_row(
        db,
        "rules",
        "guild_id, rule_id",
        vec![scope.guild_id().into(), id.to_owned().into()],
        rule.clone(),
        false,
        Some(scope),
    )
    .await
}

/// The global rule editor's save and override clear are one captured mutation.
pub async fn upsert_global_rule_and_clear_enrolled_override(
    db: &Db,
    request: ConfigurationScope,
    id: &str,
    rule: &RuleDef,
) -> Result<()> {
    anyhow::ensure!(
        request.guild_id() > 0,
        "override clearing requires a server"
    );
    anyhow::ensure!(
        crate::custom_rules::known_rule_id(id) && !crate::custom_rules::is_custom_id(id),
        "global rule must be built-in"
    );
    crate::image_spam::validate_rule(id, rule)?;
    let global = ConfigurationWriteScope::new(request.clone(), ConfigurationScope::Global)?;
    let guild = ConfigurationWriteScope::new(request.clone(), request)?;
    let id = id.to_owned();
    let json = serde_json::to_string(rule)?;
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        global.verify_conn(&tx)?;
        // The global target's check alone does not cover the guild write fence.
        guild.verify_conn(&tx)?;
        tx.execute_rows(
            "INSERT INTO rules (guild_id, rule_id, data) VALUES (0, ?1, ?2)
             ON CONFLICT(guild_id, rule_id) DO UPDATE SET data = excluded.data",
            params![id.as_str(), json],
            1,
        )?;
        tx.execute_reclaim(
            "DELETE FROM rules WHERE guild_id = ?1 AND rule_id = ?2",
            params![guild.guild_id(), id.as_str()],
        )?;
        bump_config_version_conn(&tx)?;
        tx.commit()?;
        Ok(())
    })
    .await
}

async fn upsert_enrolled_custom_rule(
    db: &Db,
    scope: ConfigurationWriteScope,
    id: &str,
    rule: &RuleDef,
) -> Result<()> {
    let guild_id = scope.guild_id();
    let json = serde_json::to_string(rule)?;
    let id_owned = id.to_owned();
    let rule = rule.clone();
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        verify_configuration_write(&tx, Some(&scope), guild_id)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM rules WHERE guild_id = ?1 AND rule_id = ?2)",
            params![guild_id, id_owned.as_str()],
            |r| Ok(r.get(0)?),
        )?;
        crate::custom_rules::validate_custom_rule_for_write(guild_id, &id_owned, &rule, !exists)?;
        if !exists {
            let count: i64 = tx.query_row(
                "SELECT COUNT(*) FROM rules WHERE guild_id = ?1 AND rule_id LIKE 'custom:%'",
                params![guild_id],
                |r| Ok(r.get(0)?),
            )?;
            crate::custom_rules::ensure_room_for_new_custom_rules(count as usize, 1)?;
        }
        tx.execute_rows(
            "INSERT INTO rules (guild_id, rule_id, data) VALUES (?1, ?2, ?3)
             ON CONFLICT(guild_id, rule_id) DO UPDATE SET data = excluded.data",
            params![guild_id, id_owned.as_str(), json],
            1,
        )?;
        bump_config_version_conn(&tx)?;
        tx.commit()?;
        Ok(())
    })
    .await
}
pub async fn delete_enrolled_rule(db: &Db, scope: ConfigurationWriteScope, id: &str) -> Result<()> {
    delete_row(
        db,
        "rules",
        "guild_id, rule_id",
        vec![scope.guild_id().into(), id.to_owned().into()],
        false,
        Some(scope),
    )
    .await
}
pub async fn upsert_enrolled_command(
    db: &Db,
    scope: ConfigurationWriteScope,
    name: &str,
    command: &CommandDef,
) -> Result<()> {
    upsert_row(
        db,
        "commands",
        "guild_id, name",
        vec![scope.guild_id().into(), name.to_owned().into()],
        command.clone(),
        true,
        Some(scope),
    )
    .await
}
pub async fn delete_enrolled_command(
    db: &Db,
    scope: ConfigurationWriteScope,
    name: &str,
) -> Result<()> {
    delete_row(
        db,
        "commands",
        "guild_id, name",
        vec![scope.guild_id().into(), name.to_owned().into()],
        true,
        Some(scope),
    )
    .await
}
pub async fn upsert_enrolled_template(
    db: &Db,
    scope: ConfigurationWriteScope,
    event: &str,
    template: &str,
) -> Result<()> {
    upsert_template_inner(db, scope.guild_id(), event, template, Some(scope)).await
}
pub async fn delete_enrolled_template(
    db: &Db,
    scope: ConfigurationWriteScope,
    event: &str,
) -> Result<()> {
    delete_row(
        db,
        "templates",
        "guild_id, event_type",
        vec![scope.guild_id().into(), event.to_owned().into()],
        false,
        Some(scope),
    )
    .await
}
pub async fn add_enrolled_badword(
    db: &Db,
    scope: ConfigurationWriteScope,
    pattern: &str,
    note: Option<&str>,
) -> Result<i64> {
    add_badword_inner(db, scope.guild_id(), pattern, note, Some(scope)).await
}
pub async fn delete_enrolled_badword(
    db: &Db,
    scope: ConfigurationWriteScope,
    id: i64,
) -> Result<()> {
    delete_badword_inner(db, id, scope.guild_id(), Some(scope)).await
}

/// Read the configuration and verify the captured membership in one primary snapshot.
pub async fn load_enrolled_web_config_snapshot(
    db: &Db,
    scope: ConfigurationScope,
) -> Result<WebConfigSnapshot> {
    let snapshot = load_web_config_snapshot_primary(db, scope.guild_id(), Some(scope)).await?;
    anyhow::ensure!(
        snapshot.version >= db.required_config_version(),
        "primary WebUI config snapshot is below the process's committed-version floor"
    );
    Ok(snapshot)
}

/// Guild and global layers from one captured primary snapshot. For Global scope
/// both fields contain the global rows, preserving the dashboard's existing shape.
#[derive(Debug)]
pub struct ConfigurationLayers<T> {
    pub guild: T,
    pub global: T,
}

async fn configuration_read<T: Send + 'static>(
    db: &Db,
    scope: ConfigurationScope,
    read: impl FnOnce(&crate::db_compat::Transaction, i64) -> Result<T> + Send + 'static,
) -> Result<T> {
    let (version, result) = db
        .with_conn(move |conn| {
            let tx = conn.transaction_primary_snapshot()?;
            scope.verify_conn(&tx)?;
            let version = get_config_version_conn(&tx)?;
            let result = read(&tx, scope.guild_id())?;
            tx.finish_read()?;
            Ok((version, result))
        })
        .await?;
    anyhow::ensure!(
        version >= db.required_config_version(),
        "primary configuration read is below the process's committed-version floor"
    );
    Ok(result)
}

fn read_layers<T: Clone>(
    guild: i64,
    read: impl Fn(i64) -> Result<T>,
) -> Result<ConfigurationLayers<T>> {
    let guild_rows = read(guild)?;
    let global = if guild == GLOBAL_SCOPE {
        guild_rows.clone()
    } else {
        read(GLOBAL_SCOPE)?
    };
    Ok(ConfigurationLayers {
        guild: guild_rows,
        global,
    })
}

/// Read only the requested rule's two layers, verifying membership even if absent.
pub async fn get_enrolled_rule(
    db: &Db,
    scope: ConfigurationScope,
    id: &str,
) -> Result<ConfigurationLayers<Option<RuleDef>>> {
    let id = id.to_owned();
    configuration_read(db, scope, move |tx, guild| {
        read_layers(guild, |g| {
            Ok(
                load_rows_conn::<RuleDef>(tx, g, "rules", "rule_id", Some(&id))?
                    .into_iter()
                    .next()
                    .map(|(_, row)| row),
            )
        })
    })
    .await
}

pub async fn list_enrolled_commands(
    db: &Db,
    scope: ConfigurationScope,
) -> Result<ConfigurationLayers<Vec<(String, CommandDef)>>> {
    configuration_read(db, scope, |tx, guild| {
        read_layers(guild, |g| load_rows_conn(tx, g, "commands", "name", None))
    })
    .await
}

pub async fn get_enrolled_command(
    db: &Db,
    scope: ConfigurationScope,
    name: &str,
) -> Result<ConfigurationLayers<Option<CommandDef>>> {
    let name = name.to_owned();
    configuration_read(db, scope, move |tx, guild| {
        read_layers(guild, |g| {
            Ok(
                load_rows_conn::<CommandDef>(tx, g, "commands", "name", Some(&name))?
                    .into_iter()
                    .next()
                    .map(|(_, row)| row),
            )
        })
    })
    .await
}

pub async fn list_enrolled_templates(
    db: &Db,
    scope: ConfigurationScope,
) -> Result<ConfigurationLayers<Vec<(String, String)>>> {
    configuration_read(db, scope, |tx, guild| {
        read_layers(guild, |g| list_templates_conn(tx, g))
    })
    .await
}

pub async fn get_enrolled_template(
    db: &Db,
    scope: ConfigurationScope,
    event: &str,
) -> Result<ConfigurationLayers<Option<String>>> {
    let event = event.to_owned();
    configuration_read(db, scope, move |tx, guild| {
        read_layers(guild, |g| get_template_row_conn(tx, g, &event))
    })
    .await
}

/// Badwords include global rows; log-channel mappings below never inherit them.
pub async fn list_enrolled_badwords(db: &Db, scope: ConfigurationScope) -> Result<Vec<Badword>> {
    configuration_read(db, scope, list_badwords_conn).await
}

pub async fn list_enrolled_log_channels(
    db: &Db,
    scope: ConfigurationScope,
) -> Result<Vec<(String, i64)>> {
    configuration_read(db, scope, list_log_channels_conn).await
}
