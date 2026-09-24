//! Local shared↔dedicated guild-store moves. Conductor copies SQL rows and
//! updates directory placement; it never imports tenant routing or Discord.
use anyhow::{Context as _, Result, ensure};
use rand::Rng;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

use crate::config::{Config, GuildStorePath};
use crate::state::Conductor;
use crate::store::{ControlStore, now_ms};

#[derive(Debug, Deserialize)]
pub struct MoveRequest {
    pub guild_id: i64,
    pub destination_store_id: String,
    #[serde(default)]
    pub preop: String,
    #[serde(default)]
    pub confirm_shared_impact: bool,
}

#[derive(Clone)]
struct Directory {
    guild: i64,
    source_id: String,
    source_revision: String,
    source_role: String,
    dest_id: String,
    dest_revision: String,
    dest_role: String,
}

pub fn moves_available(cfg: &Config) -> bool {
    cfg.capture_core_ready()
        && cfg.guild_stores.iter().any(|store| store.role == "shared")
        && cfg
            .guild_stores
            .iter()
            .any(|store| store.role == "dedicated")
        && cfg.guild_stores.len() >= 2
}

pub async fn plan(cfg: &Config, guild_id: i64, dest_id: &str) -> Result<Value> {
    ensure!(
        guild_id > 0,
        "guild id must be a positive Discord snowflake"
    );
    ensure_store_id(dest_id)?;
    let core = core_path(cfg)?;
    let conn = open(core).await?;
    let source = placement(&conn, guild_id)
        .await?
        .context("guild is not placed")?;
    let dest = catalog(&conn, dest_id)
        .await?
        .context("destination store is not registered")?;
    ensure!(
        source.0 != dest_id,
        "destination is already this guild's store"
    );
    let dest_path = store_path(cfg, dest_id)?;
    ensure!(
        dest_path.path.exists(),
        "destination database is missing; install its path in bot/API and conductor configuration first"
    );
    let source_path = store_path(cfg, &source.0)?;
    ensure!(source_path.path.exists(), "source guild store is missing");
    let source_role = catalog(&conn, &source.0)
        .await?
        .context("source store is not registered")?;
    ensure_move_pair(&conn, guild_id, &source.0, &source_role, dest_id, &dest).await?;
    let counts = tenant_counts(&source_path.path, guild_id).await?;
    let neighbors = shared_neighbors(&conn, &source.0, guild_id).await?;
    Ok(json!({
        "guild_id": guild_id.to_string(),
        "source_store_id": source.0,
        "source_revision": source.1,
        "destination_store_id": dest_id,
        "destination_role": dest,
        "counts": counts,
        "neighbor_guilds_on_source": neighbors,
        "interruption": "This guild's writes freeze until cutover. Shared bot/API processes must already have the destination store credentials; adding a new store requires restarting those services first.",
        "confirm_shared_impact": true,
        "checkpoint": "Uses an R4 checkpoint archive when capture is configured. Source tenant rows are removed after a verified cutover; rollback is restore from that checkpoint, not a placement pointer flip.",
        "discord": "Queued Discord actions are not replayed. Claim and dispatch wait until this guild resumes; overdue receipts still expire on their existing deadline.",
    }))
}

pub async fn enqueue(conductor: &Conductor, request: MoveRequest) -> Result<String> {
    ensure!(
        moves_available(&conductor.cfg),
        "guild moves are not configured"
    );
    ensure!(
        request.confirm_shared_impact,
        "confirm that bot/API already have the destination store and that this guild will be interrupted"
    );
    let preop = match request.preop.as_str() {
        "" | "fresh" => "fresh",
        "recent" => "recent",
        "skip" => "skip",
        _ => anyhow::bail!("preop must be fresh, recent, or skip"),
    };
    if preop != "skip" {
        ensure!(
            conductor.capture_available(),
            "checkpoint capture is not configured; pass preop=skip only after accepting no pre-move archive"
        );
    }
    let plan = plan(
        &conductor.cfg,
        request.guild_id,
        &request.destination_store_id,
    )
    .await?;
    ensure_new_move_allowed(conductor, request.guild_id).await?;
    let dest_revision = hex32();
    let payload = json!({
        "guild_id": request.guild_id,
        "destination_store_id": request.destination_store_id,
        "destination_revision": dest_revision,
        "preop": preop,
        "plan": plan,
    });
    conductor
        .store()?
        .insert_move_job_if_idle("guild_move", &payload, 1)
        .await?
        .context("a backup or guild-move job is already running")
}

pub async fn run(conductor: &Conductor, payload_json: &str) -> Result<()> {
    let payload: Value = serde_json::from_str(payload_json)?;
    let guild = payload
        .get("guild_id")
        .and_then(Value::as_i64)
        .context("move job missing guild id")?;
    let dest_id = payload
        .get("destination_store_id")
        .and_then(Value::as_str)
        .context("move job missing destination")?
        .to_owned();
    let dest_revision = payload
        .get("destination_revision")
        .and_then(Value::as_str)
        .context("move job missing destination revision")?
        .to_owned();
    let preop = payload
        .get("preop")
        .and_then(Value::as_str)
        .unwrap_or("fresh");
    checkpoint(conductor, preop).await?;
    let resumed =
        resumed_directory(&conductor.cfg, &payload, guild, &dest_id, &dest_revision).await?;
    let mut destination_committed = resumed.is_some();
    let directory = match resumed {
        Some(directory) => directory,
        None => {
            preflight_source_cleanup(&conductor.cfg, guild).await?;
            freeze(conductor, guild, &dest_id, &dest_revision).await?
        }
    };
    let result = copy_and_cutover(conductor, &directory, &mut destination_committed).await;
    if result.is_err() && !destination_committed {
        let _ = clear_fence(&conductor.cfg, guild, &directory.dest_revision).await;
    }
    result
}

async fn preflight_source_cleanup(cfg: &Config, guild: i64) -> Result<()> {
    let conn = open(core_path(cfg)?).await?;
    let (source_id, _) = placement(&conn, guild)
        .await?
        .context("guild is not placed")?;
    ensure_source_cleanup_budget(&store_path(cfg, &source_id)?.path, guild).await
}

async fn resumed_directory(
    cfg: &Config,
    payload: &Value,
    guild: i64,
    dest_id: &str,
    dest_revision: &str,
) -> Result<Option<Directory>> {
    let conn = open(core_path(cfg)?).await?;
    let Some((placed_id, placed_revision)) = placement(&conn, guild).await? else {
        return Ok(None);
    };
    if placed_id != dest_id || placed_revision != dest_revision {
        return Ok(None);
    }
    if let Some((fenced_id, fenced_revision)) = active_move_fence(cfg, guild).await? {
        ensure!(
            fenced_id == dest_id && fenced_revision == dest_revision,
            "retry fence does not match destination"
        );
    }
    let plan = payload
        .get("plan")
        .context("move retry lacks original plan")?;
    let source_id = plan
        .get("source_store_id")
        .and_then(Value::as_str)
        .context("move retry lacks source store")?
        .to_owned();
    let source_revision = plan
        .get("source_revision")
        .and_then(Value::as_str)
        .context("move retry lacks source revision")?
        .to_owned();
    ensure!(
        source_id != dest_id,
        "move retry source matches destination"
    );
    let source_role = catalog(&conn, &source_id)
        .await?
        .context("move retry source catalog missing")?;
    let dest_role = catalog(&conn, dest_id)
        .await?
        .context("move retry destination catalog missing")?;
    Ok(Some(Directory {
        guild,
        source_id,
        source_revision,
        source_role,
        dest_id: dest_id.to_owned(),
        dest_revision: dest_revision.to_owned(),
        dest_role,
    }))
}

async fn checkpoint(conductor: &Conductor, preop: &str) -> Result<()> {
    if preop == "skip" {
        return Ok(());
    }
    if preop == "recent"
        && let Some(last) = conductor.store()?.last_complete_ms().await?
        && now_ms().saturating_sub(last) < 6 * 3600 * 1000
    {
        return Ok(());
    }
    let settings = conductor.settings().await?;
    crate::backup::capture(
        &conductor.cfg,
        conductor.store()?,
        conductor.backup_zone()?,
        "checkpoint",
        &settings,
    )
    .await?;
    Ok(())
}

async fn ensure_new_move_allowed(conductor: &Conductor, guild_id: i64) -> Result<()> {
    if active_move_fence(&conductor.cfg, guild_id).await?.is_none() {
        return Ok(());
    }
    let store = conductor
        .store()
        .context("control store is not configured")?;
    if let Some(job) = store.latest_failed_guild_move_for_guild(guild_id).await? {
        anyhow::bail!(
            "guild {guild_id} still has a move write-fence from a failed attempt; \
             retry job {job_id} instead of starting a new move (POST /api/jobs/{job_id}/retry)",
            job_id = job.id,
        );
    }
    anyhow::bail!(
        "guild {guild_id} has an active move write-fence; wait for the in-flight move job, \
         retry its failed job record, or restore from checkpoint before starting a new move",
    );
}

async fn active_move_fence(cfg: &Config, guild: i64) -> Result<Option<(String, String)>> {
    let core = core_path(cfg)?;
    if !core.exists() {
        return Ok(None);
    }
    let conn = open(core).await?;
    let mut rows = conn
        .query(
            "SELECT destination_store_id, destination_revision FROM guild_write_fences WHERE guild_id=?",
            libsql::params![guild],
        )
        .await?;
    Ok(match rows.next().await? {
        Some(row) => Some((row.get(0)?, row.get(1)?)),
        None => None,
    })
}

pub async fn active_move_fence_guilds(cfg: &Config) -> Result<Vec<i64>> {
    let Ok(core) = core_path(cfg) else {
        return Ok(Vec::new());
    };
    if !core.exists() {
        return Ok(Vec::new());
    }
    let conn = open(core).await?;
    let mut rows = conn
        .query(
            "SELECT guild_id FROM guild_write_fences ORDER BY guild_id",
            (),
        )
        .await?;
    let mut guilds = Vec::new();
    while let Some(row) = rows.next().await? {
        guilds.push(row.get(0)?);
    }
    Ok(guilds)
}

async fn freeze(
    conductor: &Conductor,
    guild: i64,
    dest_id: &str,
    dest_revision: &str,
) -> Result<Directory> {
    let core = core_path(&conductor.cfg)?;
    let conn = open(core).await?;
    conn.execute("BEGIN IMMEDIATE", ()).await?;
    ensure_app_budget(&conn, 1_048_576).await?;
    let (source_id, source_revision) = placement(&conn, guild)
        .await?
        .context("guild is not placed")?;
    let source_role = catalog(&conn, &source_id)
        .await?
        .context("source store is not registered")?;
    let dest_role = catalog(&conn, dest_id)
        .await?
        .context("destination store is not registered")?;
    ensure_move_pair(&conn, guild, &source_id, &source_role, dest_id, &dest_role).await?;
    let queued: i64 = scalar(
        &conn,
        "SELECT EXISTS(SELECT 1 FROM guild_erasure_jobs WHERE guild_id=?)",
        libsql::params![guild],
    )
    .await?;
    ensure!(queued == 0, "guild has a pending erasure job");
    let existing = query_string(
        &conn,
        "SELECT generation || ':' || destination_store_id || ':' || destination_revision FROM guild_write_fences WHERE guild_id=?",
        libsql::params![guild],
    )
    .await?;
    if let Some(current) = existing {
        let expected = format!("{dest_revision}:{dest_id}:{dest_revision}");
        ensure!(
            current == expected,
            "guild is already fenced for a different move"
        );
        conn.execute(
            "UPDATE guild_write_fences SET phase='freeze' WHERE guild_id=?",
            libsql::params![guild],
        )
        .await?;
    } else {
        conn.execute(
            "INSERT INTO guild_write_fences(guild_id,generation,phase,destination_store_id,destination_revision,created_at)
             VALUES(?,?, 'freeze', ?, ?, unixepoch())",
            libsql::params![guild, dest_revision, dest_id, dest_revision],
        )
        .await
        .context("failed to freeze guild writes")?;
    }
    ensure_app_budget(&conn, 0).await?;
    conn.execute("COMMIT", ()).await?;
    Ok(Directory {
        guild,
        source_id,
        source_revision,
        source_role,
        dest_id: dest_id.to_owned(),
        dest_revision: dest_revision.to_owned(),
        dest_role,
    })
}

async fn copy_and_cutover(
    conductor: &Conductor,
    directory: &Directory,
    destination_committed: &mut bool,
) -> Result<()> {
    let cfg = &conductor.cfg;
    let source = store_path(cfg, &directory.source_id)?;
    let dest = store_path(cfg, &directory.dest_id)?;
    let conn = open(core_path(cfg)?).await?;
    if let Some((store_id, revision)) = placement(&conn, directory.guild).await?
        && store_id == directory.dest_id
        && revision == directory.dest_revision
    {
        ensure_guild_schema(
            &source.path,
            &directory.source_id,
            &directory.source_role,
            directory.guild,
        )
        .await?;
        ensure_guild_schema(
            &dest.path,
            &directory.dest_id,
            &directory.dest_role,
            directory.guild,
        )
        .await?;
        let source_conn = open(&source.path).await?;
        let source_bound: i64 = scalar(
            &source_conn,
            "SELECT COUNT(*) FROM guild_bindings WHERE guild_id=?",
            libsql::params![directory.guild],
        )
        .await?;
        if source_bound == 0 {
            let dest_conn = open(&dest.path).await?;
            let revision = query_string(
                &dest_conn,
                "SELECT revision FROM guild_bindings WHERE guild_id=?",
                libsql::params![directory.guild],
            )
            .await?;
            ensure!(
                revision.as_deref() == Some(directory.dest_revision.as_str()),
                "destination binding changed after source cleanup"
            );
            let snapshots: i64 = scalar(
                &dest_conn,
                "SELECT COUNT(*) FROM guild_message_snapshots WHERE guild_id=?",
                libsql::params![directory.guild],
            )
            .await?;
            ensure!(
                snapshots == 0,
                "destination retains transient message snapshots"
            );
            resume(cfg, directory).await?;
            return Ok(());
        }
        verify_copy(&source.path, &dest.path, directory.guild).await?;
        cleanup_source(&source.path, directory.guild).await?;
        resume(cfg, directory).await?;
        return Ok(());
    }
    ensure_guild_schema(
        &source.path,
        &directory.source_id,
        &directory.source_role,
        directory.guild,
    )
    .await?;
    ensure_guild_schema(
        &dest.path,
        &directory.dest_id,
        &directory.dest_role,
        directory.guild,
    )
    .await?;
    copy_guild(&source.path, &dest.path, directory).await?;
    *destination_committed = true;
    verify_copy(&source.path, &dest.path, directory.guild).await?;
    ensure_source_cleanup_budget(&source.path, directory.guild).await?;
    switch_placement(cfg, directory).await?;
    ensure_guild_schema(
        &source.path,
        &directory.source_id,
        &directory.source_role,
        directory.guild,
    )
    .await?;
    cleanup_source(&source.path, directory.guild).await?;
    resume(cfg, directory).await?;
    Ok(())
}

async fn switch_placement(cfg: &Config, directory: &Directory) -> Result<()> {
    let conn = open(core_path(cfg)?).await?;
    conn.execute("BEGIN IMMEDIATE", ()).await?;
    let switched: Result<()> = async {
        ensure_app_budget(&conn, 1_048_576).await?;
        let queued: i64 = scalar(
            &conn,
            "SELECT EXISTS(SELECT 1 FROM guild_erasure_jobs WHERE guild_id=?)",
            libsql::params![directory.guild],
        )
        .await?;
        ensure!(queued == 0, "guild has a pending erasure job");
        conn.execute(
            "UPDATE guild_write_fences SET phase='switch' WHERE guild_id=? AND destination_store_id=? AND destination_revision=?",
            libsql::params![directory.guild, directory.dest_id.as_str(), directory.dest_revision.as_str()],
        )
        .await?;
        let changed = conn
            .execute(
                "UPDATE guild_store_placements SET store_id=?, revision=? WHERE guild_id=? AND store_id=? AND revision=?",
                libsql::params![
                    directory.dest_id.as_str(),
                    directory.dest_revision.as_str(),
                    directory.guild,
                    directory.source_id.as_str(),
                    directory.source_revision.as_str()
                ],
            )
            .await?;
        ensure!(changed == 1, "placement changed during the move");
        ensure_app_budget(&conn, 0).await?;
        Ok(())
    }
    .await;
    match switched {
        Ok(()) => {
            conn.execute("COMMIT", ()).await?;
            Ok(())
        }
        Err(error) => {
            let _ = conn.execute("ROLLBACK", ()).await;
            Err(error)
        }
    }
}

async fn resume(cfg: &Config, directory: &Directory) -> Result<()> {
    clear_fence(cfg, directory.guild, &directory.dest_revision).await
}

pub async fn clear_stale_fences(cfg: &Config, store: &ControlStore) -> Result<()> {
    if store.has_active_move_job().await? {
        return Ok(());
    }
    let Ok(core) = core_path(cfg) else {
        return Ok(());
    };
    if !core.exists() {
        return Ok(());
    }
    let conn = open(core).await?;
    // Only drop fences left after a completed cutover (placement matches destination).
    // Failed moves that copied to the destination while placement still points at the
    // source must keep the fence until retry or restore.
    conn.execute("BEGIN IMMEDIATE", ()).await?;
    let cleared: Result<()> = async {
        let before = allocated_bytes(&conn).await?;
        ensure!(
            before <= 984_000_000,
            "fence cleanup exceeds recovery ceiling"
        );
        conn.execute(
            "DELETE FROM guild_write_fences WHERE EXISTS (
            SELECT 1 FROM guild_store_placements p
            WHERE p.guild_id = guild_write_fences.guild_id
              AND p.store_id = guild_write_fences.destination_store_id
              AND p.revision = guild_write_fences.destination_revision
        )",
            (),
        )
        .await?;
        ensure!(
            allocated_bytes(&conn).await? <= before,
            "fence cleanup allocated database pages"
        );
        Ok(())
    }
    .await;
    match cleared {
        Ok(()) => {
            conn.execute("COMMIT", ()).await?;
            Ok(())
        }
        Err(error) => {
            let _ = conn.execute("ROLLBACK", ()).await;
            Err(error)
        }
    }
}

async fn clear_fence(cfg: &Config, guild: i64, generation: &str) -> Result<()> {
    let conn = open(core_path(cfg)?).await?;
    conn.execute("BEGIN IMMEDIATE", ()).await?;
    let cleared: Result<()> = async {
        let before = allocated_bytes(&conn).await?;
        ensure!(
            before <= 984_000_000,
            "fence cleanup exceeds recovery ceiling"
        );
        conn.execute(
            "DELETE FROM guild_write_fences WHERE guild_id=? AND generation=?",
            libsql::params![guild, generation],
        )
        .await?;
        ensure!(
            allocated_bytes(&conn).await? <= before,
            "fence cleanup allocated database pages"
        );
        Ok(())
    }
    .await;
    match cleared {
        Ok(()) => {
            conn.execute("COMMIT", ()).await?;
            Ok(())
        }
        Err(error) => {
            let _ = conn.execute("ROLLBACK", ()).await;
            Err(error)
        }
    }
}

async fn copy_guild(source: &Path, dest: &Path, directory: &Directory) -> Result<()> {
    let src = open(source).await?;
    let dest_conn = open(dest).await?;
    let guild = directory.guild;
    let bound: i64 = scalar(
        &dest_conn,
        "SELECT EXISTS(SELECT 1 FROM guild_bindings WHERE guild_id=?)",
        libsql::params![guild],
    )
    .await?;
    if bound == 1 {
        let revision = query_string(
            &dest_conn,
            "SELECT revision FROM guild_bindings WHERE guild_id=?",
            libsql::params![guild],
        )
        .await?
        .unwrap_or_default();
        ensure!(
            revision == directory.dest_revision,
            "destination already has a different binding for this guild; clean it before retrying"
        );
        return Ok(());
    }
    // Hold the source primary snapshot from measurement through copying.
    src.execute("BEGIN IMMEDIATE", ()).await?;
    if let Err(error) = dest_conn.execute("BEGIN IMMEDIATE", ()).await {
        let _ = src.execute("ROLLBACK", ()).await;
        return Err(error.into());
    }
    let copied: Result<()> = async {
        // Only selected durable rows enter the destination. Source free pages,
        // other guilds and transient snapshots consume none of its copy budget.
        let copy_bound = selected_copy_budget(&src, guild).await?;
        ensure_app_budget(&dest_conn, copy_bound).await?;
        dest_conn
            .execute(
                "INSERT INTO guild_bindings(guild_id,revision) VALUES(?,?)",
                libsql::params![guild, directory.dest_revision.as_str()],
            )
            .await?;
        let mut membership = src
            .query(
                "SELECT enrollment_id,joined_at_ns,active,config_revision FROM guild_memberships WHERE guild_id=?",
                libsql::params![guild],
            )
            .await?;
        let row = membership.next().await?.context("source has no membership")?;
        ensure!(row.get::<i64>(2)? == 1, "source membership is not active");
        dest_conn
            .execute(
                "INSERT INTO guild_memberships(guild_id,enrollment_id,joined_at_ns,active,config_revision) VALUES(?,?,?,1,?)",
                libsql::params![
                    guild,
                    row.get::<String>(0)?,
                    row.get::<i64>(1)?,
                    row.get::<String>(3)?
                ],
            )
            .await?;
        // Message snapshots are transient: a move begins new observation coverage.
        // Never copy them into the destination, including on a resumed copy.
        copy_simple(
            &src,
            &dest_conn,
            "SELECT event_type,enrollment_id,channel_id FROM guild_log_channels WHERE guild_id=?",
            "INSERT INTO guild_log_channels(guild_id,event_type,enrollment_id,channel_id) VALUES(?,?,?,?)",
            guild,
            3,
        )
        .await?;
        if table_exists(&src, "guild_tenant_settings").await? {
            let mut rows = src
                .query(
                    "SELECT enrollment_id,case_delete_roles,revision FROM guild_tenant_settings WHERE guild_id=?",
                    libsql::params![guild],
                )
                .await?;
            while let Some(row) = rows.next().await? {
                dest_conn
                    .execute(
                        "INSERT INTO guild_tenant_settings(guild_id,enrollment_id,case_delete_roles,revision) VALUES(?,?,?,?)",
                        libsql::params![
                            guild,
                            row.get::<String>(0)?,
                            row.get::<String>(1)?,
                            row.get::<String>(2)?
                        ],
                    )
                    .await?;
            }
        }
        copy_cases(&src, &dest_conn, guild).await?;
        copy_history(&src, &dest_conn, guild).await?;
        copy_extra_sources(&src, &dest_conn, guild).await?;
        copy_live_evidence(&src, &dest_conn, guild).await?;
        ensure_app_budget(&dest_conn, 0).await?;
        Ok(())
    }
    .await;
    // Release the read-only source transaction on both success and refusal.
    let source_released = src.execute("ROLLBACK", ()).await;
    let copied = copied.and_then(|()| source_released.map(|_| ()).map_err(Into::into));
    match copied {
        Ok(()) => {
            dest_conn.execute("COMMIT", ()).await?;
            Ok(())
        }
        Err(error) => {
            let _ = dest_conn.execute("ROLLBACK", ()).await;
            Err(error)
        }
    }
}

async fn copy_cases(src: &libsql::Connection, dest: &libsql::Connection, guild: i64) -> Result<()> {
    let mut rows = src
        .query(
            "SELECT enrollment_id,case_id,source_key,payload,deleted FROM guild_cases WHERE guild_id=?",
            libsql::params![guild],
        )
        .await?;
    while let Some(row) = rows.next().await? {
        dest.execute(
            "INSERT INTO guild_cases(guild_id,enrollment_id,case_id,source_key,payload,deleted) VALUES(?,?,?,?,?,?)",
            libsql::params![
                guild,
                row.get::<String>(0)?,
                row.get::<String>(1)?,
                row.get::<String>(2)?,
                row.get::<Option<String>>(3)?,
                row.get::<i64>(4)?
            ],
        )
        .await?;
    }
    Ok(())
}

async fn copy_history(
    src: &libsql::Connection,
    dest: &libsql::Connection,
    guild: i64,
) -> Result<()> {
    if !table_exists(src, "guild_case_history").await? {
        return Ok(());
    }
    let mut rows = src
        .query(
            "SELECT enrollment_id,case_id,entry_id,sequence,payload FROM guild_case_history WHERE guild_id=?",
            libsql::params![guild],
        )
        .await?;
    while let Some(row) = rows.next().await? {
        dest.execute(
            "INSERT INTO guild_case_history(guild_id,enrollment_id,case_id,entry_id,sequence,payload) VALUES(?,?,?,?,?,?)",
            libsql::params![
                guild,
                row.get::<String>(0)?,
                row.get::<String>(1)?,
                row.get::<String>(2)?,
                row.get::<i64>(3)?,
                row.get::<String>(4)?
            ],
        )
        .await?;
    }
    Ok(())
}

async fn copy_extra_sources(
    src: &libsql::Connection,
    dest: &libsql::Connection,
    guild: i64,
) -> Result<()> {
    if !table_exists(src, "guild_case_sources").await? {
        return Ok(());
    }
    let mut rows = src
        .query(
            "SELECT s.enrollment_id,s.case_id,s.source_key,s.source_kind
             FROM guild_case_sources s
             JOIN guild_cases c ON c.guild_id=s.guild_id AND c.enrollment_id=s.enrollment_id AND c.case_id=s.case_id
             WHERE s.guild_id=? AND s.source_key != c.source_key",
            libsql::params![guild],
        )
        .await?;
    while let Some(row) = rows.next().await? {
        dest.execute(
            "INSERT INTO guild_case_sources(guild_id,enrollment_id,case_id,source_key,source_kind) VALUES(?,?,?,?,?)",
            libsql::params![
                guild,
                row.get::<String>(0)?,
                row.get::<String>(1)?,
                row.get::<String>(2)?,
                row.get::<i64>(3)?
            ],
        )
        .await?;
    }
    Ok(())
}

async fn copy_live_evidence(
    src: &libsql::Connection,
    dest: &libsql::Connection,
    guild: i64,
) -> Result<()> {
    if !table_exists(src, "guild_case_evidence").await? {
        return Ok(());
    }
    let mut rows = src
        .query(
            "SELECT enrollment_id,case_id,evidence_id,storage_id,reserved_bytes,state,cancelled,channel_id,message_id,attachment_id,sha256
             FROM guild_case_evidence WHERE guild_id=? AND cancelled=0 AND state IN (0,1)",
            libsql::params![guild],
        )
        .await?;
    while let Some(row) = rows.next().await? {
        let enrollment: String = row.get(0)?;
        let case_id: String = row.get(1)?;
        let evidence_id: String = row.get(2)?;
        let storage_id: String = row.get(3)?;
        let reserved: i64 = row.get(4)?;
        let state: i64 = row.get(5)?;
        let channel: i64 = row.get(7)?;
        let message: i64 = row.get(8)?;
        let attachment: i64 = row.get(9)?;
        let sha256: Option<String> = row.get(10)?;
        dest.execute(
            "INSERT INTO guild_case_evidence(guild_id,enrollment_id,case_id,evidence_id,storage_id,reserved_bytes,state,cancelled,channel_id,message_id,attachment_id,sha256)
             VALUES(?,?,?,?,?,?,0,0,?,?,?,NULL)",
            libsql::params![
                guild,
                enrollment.as_str(),
                case_id.as_str(),
                evidence_id.as_str(),
                storage_id.as_str(),
                reserved,
                channel,
                message,
                attachment
            ],
        )
        .await?;
        if let Some(binding) = bunny_binding(src, guild, &enrollment, &evidence_id).await? {
            dest.execute(
                "INSERT INTO guild_bunny_evidence(guild_id,enrollment_id,evidence_id,storage_id,binding,sha256,accepted) VALUES(?,?,?,?,?,?,0)",
                libsql::params![
                    guild,
                    enrollment.as_str(),
                    evidence_id.as_str(),
                    storage_id.as_str(),
                    binding.as_str(),
                    sha256.clone()
                ],
            )
            .await?;
            if state == 1 {
                dest.execute(
                    "UPDATE guild_bunny_evidence SET accepted=1 WHERE guild_id=? AND evidence_id=?",
                    libsql::params![guild, evidence_id.as_str()],
                )
                .await?;
            }
        }
        if state == 1 {
            dest.execute(
                "UPDATE guild_case_evidence SET state=1, sha256=? WHERE guild_id=? AND evidence_id=?",
                libsql::params![sha256.unwrap_or_default(), guild, evidence_id.as_str()],
            )
            .await?;
        }
    }
    Ok(())
}

async fn bunny_binding(
    src: &libsql::Connection,
    guild: i64,
    enrollment: &str,
    evidence_id: &str,
) -> Result<Option<String>> {
    if !table_exists(src, "guild_bunny_evidence").await? {
        return Ok(None);
    }
    query_string(
        src,
        "SELECT binding FROM guild_bunny_evidence WHERE guild_id=? AND enrollment_id=? AND evidence_id=?",
        libsql::params![guild, enrollment, evidence_id],
    )
    .await
}

async fn cleanup_source(path: &Path, guild: i64) -> Result<()> {
    loop {
        let conn = open(path).await?;
        conn.execute("BEGIN IMMEDIATE", ()).await?;
        let cleaned: Result<bool> = async {
            let before = allocated_bytes(&conn).await?;
            ensure_cleanup_budget(&conn, guild).await?;
            if table_exists(&conn, "guild_message_snapshots").await? {
                let left = scalar(
                    &conn,
                    "SELECT COUNT(*) FROM guild_message_snapshots WHERE guild_id=?",
                    libsql::params![guild],
                )
                .await?;
                if left > 0 {
                    let removed = conn
                        .execute(
                            "DELETE FROM guild_message_snapshots WHERE (guild_id,enrollment_id,channel_id,message_id) IN (SELECT guild_id,enrollment_id,channel_id,message_id FROM guild_message_snapshots WHERE guild_id=?1 LIMIT 100)",
                            libsql::params![guild],
                        )
                        .await?;
                    ensure!(removed > 0, "snapshot cleanup made no progress");
                    ensure!(
                        allocated_bytes(&conn).await? <= before,
                        "source cleanup allocated database pages"
                    );
                    return Ok(false);
                }
            }
            conn.execute(
            "DELETE FROM guild_log_channels WHERE guild_id=?",
            libsql::params![guild],
        )
        .await?;
        conn.execute(
            "UPDATE guild_memberships SET active=0 WHERE guild_id=?",
            libsql::params![guild],
        )
        .await?;
        if table_exists(&conn, "guild_case_evidence").await? {
            conn.execute(
                "UPDATE guild_case_evidence SET cancelled=1, state=CASE state WHEN 0 THEN 0 ELSE 2 END, channel_id=NULL, message_id=NULL, attachment_id=NULL, sha256=NULL WHERE guild_id=? AND cancelled=0",
                libsql::params![guild],
            )
            .await?;
            conn.execute(
                "UPDATE guild_case_evidence SET state=2 WHERE guild_id=? AND state=0",
                libsql::params![guild],
            )
            .await?;
            conn.execute(
                "UPDATE guild_case_evidence SET state=3 WHERE guild_id=? AND state=2",
                libsql::params![guild],
            )
            .await?;
        }
        conn.execute("DELETE FROM guild_cases WHERE guild_id=?", libsql::params![guild])
            .await?;
        if table_exists(&conn, "guild_tenant_settings").await? {
            conn.execute(
                "DELETE FROM guild_tenant_settings WHERE guild_id=?",
                libsql::params![guild],
            )
            .await?;
        }
        conn.execute(
            "DELETE FROM guild_memberships WHERE guild_id=? AND active=0",
            libsql::params![guild],
        )
        .await?;
        conn.execute(
            "DELETE FROM guild_bindings WHERE guild_id=?",
            libsql::params![guild],
        )
        .await?;
        ensure!(
            allocated_bytes(&conn).await? <= before,
            "source cleanup allocated database pages"
        );
        Ok(true)
    }
    .await;
        match cleaned {
            Ok(finished) => {
                conn.execute("COMMIT", ()).await?;
                if finished {
                    return Ok(());
                }
            }
            Err(error) => {
                let _ = conn.execute("ROLLBACK", ()).await;
                return Err(error);
            }
        }
    }
}

async fn verify_copy(source: &Path, dest: &Path, guild: i64) -> Result<()> {
    let expected = tenant_counts(source, guild).await?;
    let got = tenant_counts(dest, guild).await?;
    let dest_conn = open(dest).await?;
    if table_exists(&dest_conn, "guild_message_snapshots").await? {
        let snapshots = scalar(
            &dest_conn,
            "SELECT COUNT(*) FROM guild_message_snapshots WHERE guild_id=?",
            libsql::params![guild],
        )
        .await?;
        ensure!(
            snapshots == 0,
            "destination retains transient message snapshots"
        );
    }
    ensure!(
        expected.get("cases") == got.get("cases")
            && expected.get("memberships") == got.get("memberships")
            && expected.get("log_channels") == got.get("log_channels")
            && expected.get("history") == got.get("history"),
        "destination counts do not match the frozen source"
    );
    Ok(())
}

async fn tenant_counts(path: &Path, guild: i64) -> Result<Value> {
    let conn = open(path).await?;
    let memberships = scalar(
        &conn,
        "SELECT COUNT(*) FROM guild_memberships WHERE guild_id=?",
        libsql::params![guild],
    )
    .await
    .unwrap_or(0);
    let logs = scalar(
        &conn,
        "SELECT COUNT(*) FROM guild_log_channels WHERE guild_id=?",
        libsql::params![guild],
    )
    .await
    .unwrap_or(0);
    let cases = if table_exists(&conn, "guild_cases").await? {
        scalar(
            &conn,
            "SELECT COUNT(*) FROM guild_cases WHERE guild_id=?",
            libsql::params![guild],
        )
        .await?
    } else {
        0
    };
    let history = if table_exists(&conn, "guild_case_history").await? {
        scalar(
            &conn,
            "SELECT COUNT(*) FROM guild_case_history WHERE guild_id=?",
            libsql::params![guild],
        )
        .await?
    } else {
        0
    };
    Ok(json!({
        "memberships": memberships,
        "log_channels": logs,
        "cases": cases,
        "history": history,
    }))
}

async fn ensure_guild_schema(path: &Path, store_id: &str, role: &str, guild: i64) -> Result<()> {
    let conn = open(path).await?;
    ensure!(
        scalar(&conn, "PRAGMA user_version", ()).await? == 9,
        "guild move requires snapshot-aware guild schema 9"
    );
    let family = query_string(
        &conn,
        "SELECT schema_family FROM store_identity WHERE singleton=1",
        (),
    )
    .await?
    .context("destination is not a guild store")?;
    ensure!(family == "jmodguard-guild-1", "destination family mismatch");
    let id = query_string(
        &conn,
        "SELECT store_id FROM store_identity WHERE singleton=1",
        (),
    )
    .await?
    .context("destination identity missing")?;
    ensure!(
        id == store_id,
        "destination identity does not match the catalog"
    );
    let owner: Option<i64> = {
        let mut rows = conn
            .query(
                "SELECT dedicated_guild_id FROM store_identity WHERE singleton=1",
                (),
            )
            .await?;
        rows.next()
            .await?
            .and_then(|row| row.get::<Option<i64>>(0).ok())
            .flatten()
    };
    if role == "dedicated" {
        ensure!(owner == Some(guild), "dedicated store owner mismatch");
    } else {
        ensure!(owner.is_none(), "shared destination is marked dedicated");
    }
    Ok(())
}

pub async fn catalog_view(cfg: &Config) -> Result<Value> {
    let core = core_path(cfg)?;
    let conn = open(core).await?;
    let mut stores = Vec::new();
    let mut rows = conn
        .query(
            "SELECT store_id, dedicated_guild_id FROM guild_store_catalog ORDER BY store_id",
            (),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        let id: String = row.get(0)?;
        let dedicated: Option<i64> = row.get(1).ok();
        let configured = cfg.guild_stores.iter().any(|store| store.id == id);
        stores.push(json!({
            "store_id": id,
            "role": if dedicated.is_some() { "dedicated" } else { "shared" },
            "dedicated_guild_id": dedicated.map(|id| id.to_string()),
            "configured": configured,
        }));
    }
    let mut placements = Vec::new();
    let mut rows = conn
        .query(
            "SELECT guild_id, store_id, revision FROM guild_store_placements ORDER BY guild_id",
            (),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        placements.push(json!({
            "guild_id": row.get::<i64>(0)?.to_string(),
            "store_id": row.get::<String>(1)?,
            "revision": row.get::<String>(2)?,
        }));
    }
    Ok(json!({ "stores": stores, "placements": placements }))
}

fn core_path(cfg: &Config) -> Result<&Path> {
    cfg.capture_stores
        .iter()
        .find(|store| store.role == "core")
        .map(|store| store.path.as_path())
        .context("core capture store is not configured")
}

fn store_path<'a>(cfg: &'a Config, id: &str) -> Result<&'a GuildStorePath> {
    cfg.guild_stores
        .iter()
        .find(|store| store.id == id)
        .context("destination store is not in JMODGUARD_CONDUCTOR_GUILD_STORES")
}

fn ensure_store_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 32
            && id
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "invalid store id"
    );
    Ok(())
}

fn hex32() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn open(path: &Path) -> Result<libsql::Connection> {
    let db = libsql::Builder::new_local(path)
        .build()
        .await
        .with_context(|| format!("failed to open {}", path.display()))?;
    db.connect().context("failed to connect")
}

const APP_GROWTH_STOP_BYTES: u64 = 900_000_000;

// The bound covers table/index pages and split slack for the selected rows.
// It is deliberately conservative; no unselected payload enters the estimate.
async fn selected_copy_budget(conn: &libsql::Connection, guild: i64) -> Result<u64> {
    let page_size = scalar(conn, "PRAGMA page_size", ()).await?;
    ensure!(page_size > 0, "database page size unavailable");
    let mut bound = (page_size as u64)
        .checked_mul(1024)
        .context("copy budget overflow")?;
    for (table, predicate) in [
        ("guild_bindings", "guild_id=?"),
        ("guild_memberships", "guild_id=?"),
        ("guild_log_channels", "guild_id=?"),
        ("guild_tenant_settings", "guild_id=?"),
        ("guild_cases", "guild_id=?"),
        ("guild_case_history", "guild_id=?"),
        // Includes original sources created by the case insert trigger.
        ("guild_case_sources", "guild_id=?"),
        (
            "guild_case_evidence",
            "guild_id=? AND cancelled=0 AND state IN (0,1)",
        ),
        (
            "guild_bunny_evidence",
            "guild_id=? AND EXISTS(SELECT 1 FROM guild_case_evidence e WHERE e.guild_id=guild_bunny_evidence.guild_id AND e.enrollment_id=guild_bunny_evidence.enrollment_id AND e.evidence_id=guild_bunny_evidence.evidence_id AND e.cancelled=0 AND e.state IN (0,1))",
        ),
    ] {
        if !table_exists(conn, table).await? {
            continue;
        }
        let mut columns = conn
            .query(&format!("PRAGMA table_info({table})"), ())
            .await?;
        let mut lengths = Vec::new();
        while let Some(column) = columns.next().await? {
            let name = column.get::<String>(1)?.replace('"', "\"\"");
            lengths.push(format!("COALESCE(length(CAST(\"{name}\" AS BLOB)),0)+16"));
        }
        ensure!(!lengths.is_empty(), "copy table columns unavailable");
        let payload = scalar(
            conn,
            &format!(
                "SELECT COALESCE(SUM({}),0) FROM {table} WHERE {predicate}",
                lengths.join("+")
            ),
            libsql::params![guild],
        )
        .await?;
        let count = scalar(
            conn,
            &format!("SELECT COUNT(*) FROM {table} WHERE {predicate}"),
            libsql::params![guild],
        )
        .await?;
        let indexes = scalar(
            conn,
            "SELECT COUNT(*) FROM sqlite_schema WHERE type='index' AND tbl_name=?",
            libsql::params![table],
        )
        .await?;
        ensure!(
            payload >= 0 && count >= 0 && indexes >= 0,
            "copy measurement invalid"
        );
        // Every row may need separate table/index pages. Include twice the
        // payload in each index as an upper bound, even for non-covering indexes.
        let row_pages = (count as u64)
            .checked_mul(page_size as u64)
            .and_then(|n| n.checked_mul(2))
            .context("copy budget overflow")?;
        let bytes = (payload as u64)
            .checked_add(row_pages)
            .and_then(|n| n.checked_mul(indexes as u64 + 1))
            .and_then(|n| n.checked_mul(2))
            .context("copy budget overflow")?;
        bound = bound.checked_add(bytes).context("copy budget overflow")?;
    }
    Ok(bound)
}

async fn ensure_source_cleanup_budget(path: &Path, guild: i64) -> Result<()> {
    let conn = open(path).await?;
    conn.execute("BEGIN IMMEDIATE", ()).await?;
    let result = ensure_cleanup_budget(&conn, guild).await;
    let released = conn.execute("ROLLBACK", ()).await;
    result?;
    released?;
    Ok(())
}

async fn ensure_cleanup_budget(conn: &libsql::Connection, guild: i64) -> Result<()> {
    let page_size = scalar(conn, "PRAGMA page_size", ()).await?;
    ensure!(page_size > 0, "database page size unavailable");
    // Deletions need no payload copy. Evidence transitions can move keys in
    // their three secondary indexes before deletion; bound those selected rows
    // separately, plus a fixed margin for membership/snapshot-health metadata.
    let evidence = if table_exists(conn, "guild_case_evidence").await? {
        scalar(
            conn,
            "SELECT COUNT(*) FROM guild_case_evidence WHERE guild_id=?",
            libsql::params![guild],
        )
        .await?
    } else {
        0
    };
    ensure!(evidence >= 0, "cleanup measurement invalid");
    let reserve = (evidence as u64)
        .checked_mul(24)
        .and_then(|n| n.checked_add(1024))
        .and_then(|n| n.checked_mul(page_size as u64))
        .context("cleanup budget overflow")?;
    ensure!(
        allocated_bytes(conn)
            .await?
            .checked_add(reserve)
            .is_some_and(|n| n <= 984_000_000),
        "source cleanup exceeds recovery ceiling"
    );
    Ok(())
}

async fn allocated_bytes(conn: &libsql::Connection) -> Result<u64> {
    let pages = scalar(conn, "PRAGMA page_count", ()).await?;
    let page_size = scalar(conn, "PRAGMA page_size", ()).await?;
    ensure!(
        pages >= 0 && page_size > 0,
        "database capacity measurement unavailable"
    );
    (pages as u64)
        .checked_mul(page_size as u64)
        .context("database allocation overflow")
}

async fn ensure_app_budget(conn: &libsql::Connection, reserved_growth: u64) -> Result<()> {
    let allocated = allocated_bytes(conn).await?;
    ensure!(
        allocated
            .checked_add(reserved_growth)
            .is_some_and(|total| total <= APP_GROWTH_STOP_BYTES),
        "application database capacity budget exhausted"
    );
    Ok(())
}

async fn placement(conn: &libsql::Connection, guild: i64) -> Result<Option<(String, String)>> {
    let mut rows = conn
        .query(
            "SELECT store_id, revision FROM guild_store_placements WHERE guild_id=?",
            libsql::params![guild],
        )
        .await?;
    Ok(match rows.next().await? {
        Some(row) => Some((row.get(0)?, row.get(1)?)),
        None => None,
    })
}

async fn catalog(conn: &libsql::Connection, store_id: &str) -> Result<Option<String>> {
    let mut rows = conn
        .query(
            "SELECT dedicated_guild_id FROM guild_store_catalog WHERE store_id=?",
            libsql::params![store_id],
        )
        .await?;
    Ok(rows
        .next()
        .await?
        .map(|row| match row.get::<Option<i64>>(0).ok().flatten() {
            Some(_) => "dedicated".into(),
            None => "shared".into(),
        }))
}

async fn dedicated_owner(conn: &libsql::Connection, store_id: &str) -> Result<Option<i64>> {
    let mut rows = conn
        .query(
            "SELECT dedicated_guild_id FROM guild_store_catalog WHERE store_id=?",
            libsql::params![store_id],
        )
        .await?;
    Ok(rows.next().await?.and_then(|row| row.get(0).ok()))
}

async fn ensure_move_pair(
    conn: &libsql::Connection,
    guild: i64,
    source_id: &str,
    source_role: &str,
    dest_id: &str,
    dest_role: &str,
) -> Result<()> {
    ensure!(
        (source_role == "shared" && dest_role == "dedicated")
            || (source_role == "dedicated" && dest_role == "shared"),
        "moves only support shared ↔ dedicated store pairs"
    );
    if dest_role == "dedicated" {
        ensure!(
            dedicated_owner(conn, dest_id).await? == Some(guild),
            "dedicated destination belongs to another guild"
        );
    }
    if source_role == "dedicated" {
        ensure!(
            dedicated_owner(conn, source_id).await? == Some(guild),
            "dedicated source belongs to another guild"
        );
    }
    Ok(())
}

async fn shared_neighbors(
    conn: &libsql::Connection,
    store_id: &str,
    guild: i64,
) -> Result<Vec<String>> {
    let mut rows = conn
        .query(
            "SELECT guild_id FROM guild_store_placements WHERE store_id=? AND guild_id!=? ORDER BY guild_id",
            libsql::params![store_id, guild],
        )
        .await?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        out.push(row.get::<i64>(0)?.to_string());
    }
    Ok(out)
}

async fn table_exists(conn: &libsql::Connection, name: &str) -> Result<bool> {
    let mut rows = conn
        .query(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=? LIMIT 1",
            libsql::params![name],
        )
        .await?;
    Ok(rows.next().await?.is_some())
}

async fn scalar(
    conn: &libsql::Connection,
    sql: &str,
    params: impl libsql::params::IntoParams,
) -> Result<i64> {
    let mut rows = conn.query(sql, params).await?;
    Ok(rows.next().await?.context("missing scalar")?.get(0)?)
}

async fn query_string(
    conn: &libsql::Connection,
    sql: &str,
    params: impl libsql::params::IntoParams,
) -> Result<Option<String>> {
    let mut rows = conn.query(sql, params).await?;
    Ok(rows.next().await?.and_then(|row| row.get(0).ok()))
}

async fn copy_simple(
    src: &libsql::Connection,
    dest: &libsql::Connection,
    select: &str,
    insert: &str,
    guild: i64,
    values: usize,
) -> Result<()> {
    let mut rows = src.query(select, libsql::params![guild]).await?;
    while let Some(row) = rows.next().await? {
        match values {
            3 => {
                dest.execute(
                    insert,
                    libsql::params![
                        guild,
                        row.get::<String>(0)?,
                        row.get::<String>(1)?,
                        row.get::<i64>(2)?
                    ],
                )
                .await?;
            }
            _ => anyhow::bail!("unsupported copy width"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod capacity_tests {
    use super::*;

    #[tokio::test]
    async fn copy_bound_counts_selected_durable_rows_and_refuses_real_growth() {
        let db = libsql::Builder::new_local(":memory:")
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        conn.execute_batch("CREATE TABLE guild_cases(guild_id INTEGER, payload TEXT); CREATE TABLE guild_message_snapshots(guild_id INTEGER, content TEXT);").await.unwrap();
        let before_unrelated = selected_copy_budget(&conn, 10).await.unwrap();
        conn.execute("INSERT INTO guild_cases VALUES(20,zeroblob(100000))", ())
            .await
            .unwrap();
        conn.execute(
            "INSERT INTO guild_message_snapshots VALUES(10,zeroblob(100000))",
            (),
        )
        .await
        .unwrap();
        let empty = selected_copy_budget(&conn, 10).await.unwrap();
        assert_eq!(empty, before_unrelated);
        conn.execute("INSERT INTO guild_cases VALUES(10,zeroblob(100000))", ())
            .await
            .unwrap();
        let selected = selected_copy_budget(&conn, 10).await.unwrap();
        assert!(selected > empty + 200_000);
        // A transaction whose required growth would exceed the ordinary ceiling
        // remains refused, regardless of how small the source allocation is.
        let allocated = allocated_bytes(&conn).await.unwrap();
        assert!(
            ensure_app_budget(&conn, APP_GROWTH_STOP_BYTES - allocated + 1)
                .await
                .is_err()
        );
        assert!(ensure_app_budget(&conn, selected).await.is_ok());
    }
}
