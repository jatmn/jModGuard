//! Stable, restricted update control. Application schema checks stay in `Db`.
//! Future application migrations must retain the v1 control tables and keys.
use super::*;
use serde::Deserialize;

pub const UPDATE_CONTROL_PROTOCOL: u32 = 3;
/// Monotonic updater capability, independent of source SHA and shell protocol.
/// Bump when shipping launcher changes; raise the minimum only when required.
pub const LAUNCHER_REVISION: u32 = 2;
pub const REQUIRED_LAUNCHER_REVISION: u32 = 1;
const UPGRADE_KEY: &str = "component_upgrade_v1";

/// A controller restarted, or an owner replaced/cancelled the operation while
/// local work was in flight. Retry from durable state; never infer failure or
/// permission to spawn from an unsuccessful reservation.
#[derive(Debug, thiserror::Error)]
#[error("upgrade progress changed; retry from current state")]
pub struct UpgradeProgressChanged;

fn require_progress(current: bool) -> Result<()> {
    if !current {
        return Err(UpgradeProgressChanged.into());
    }
    Ok(())
}

/// This handle exposes only update operations. It never runs application
/// migrations or returns an unchecked application database to its caller.
#[derive(Clone)]
pub struct UpdateControl {
    db: Db,
}

/// Immutable application compatibility of this container's baked image. A
/// downloaded launcher may be compiled for a different application schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BakedCompatibility {
    pub source_sha: String,
    pub core: i64,
    pub guild: i64,
}

impl BakedCompatibility {
    pub fn compiled(source_sha: &str) -> Self {
        Self {
            source_sha: source_sha.into(),
            core: CORE_SCHEMA_VERSION,
            guild: guild_data::SCHEMA_VERSION,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpgradeCompatibility {
    pub control_protocol: u32,
    pub min_core: i64,
    pub min_guild: i64,
    pub guild_schema: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpgradePhase {
    Preparing,
    StoppingApi,
    StoppingBot,
    StartingBot,
    StartingApi,
    Complete,
    Aborted,
}

impl UpgradePhase {
    pub fn active(self) -> bool {
        !matches!(self, Self::Complete | Self::Aborted)
    }

    /// Persisted before the new bot can run: any core or guild migration may
    /// have happened even when the child never produced its readiness marker.
    pub fn forward_only(self) -> bool {
        matches!(self, Self::StartingBot | Self::StartingApi | Self::Complete)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradeService {
    pub release: ComponentRelease,
    pub instance: String,
    pub prepared: bool,
    pub stopped: bool,
    pub ready: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceUpgrade {
    pub id: String,
    pub phase: UpgradePhase,
    pub bot: UpgradeService,
    pub api: UpgradeService,
    pub from_core: i64,
    pub from_guild: i64,
    pub irreversible: bool,
    pub error: Option<String>,
}

impl ServiceUpgrade {
    pub fn service(&self, role: &str) -> Result<&UpgradeService> {
        match role {
            "bot" => Ok(&self.bot),
            "api" => Ok(&self.api),
            _ => anyhow::bail!("invalid service role"),
        }
    }

    fn service_mut(&mut self, role: &str) -> Result<&mut UpgradeService> {
        match role {
            "bot" => Ok(&mut self.bot),
            "api" => Ok(&mut self.api),
            _ => anyhow::bail!("invalid service role"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum UpgradeAcknowledgement {
    Prepared,
    Stopped,
    Starting,
    Ready,
}

fn read_upgrade(conn: &impl SqlExecutor) -> Result<Option<ServiceUpgrade>> {
    meta_string_conn(conn, UPGRADE_KEY)?
        .map(|raw| {
            anyhow::ensure!(raw.len() <= 32 * 1024, "oversized upgrade record");
            Ok(serde_json::from_str(&raw)?)
        })
        .transpose()
}

fn write_upgrade(conn: &impl SqlExecutor, upgrade: &ServiceUpgrade) -> Result<()> {
    let raw = serde_json::to_string(upgrade)?;
    anyhow::ensure!(raw.len() <= 32 * 1024, "oversized upgrade record");
    write_meta_string_conn(conn, UPGRADE_KEY, &raw)
}

pub(super) fn ensure_no_service_upgrade(conn: &impl SqlExecutor) -> Result<()> {
    anyhow::ensure!(
        read_upgrade(conn)?.is_none_or(|upgrade| !upgrade.phase.active()),
        "a coordinated service upgrade is in progress"
    );
    Ok(())
}

impl UpdateControl {
    /// None means verified empty or an interrupted initial bootstrap without
    /// any managed selections, where the baked bot may finish initialization.
    /// An error never authorizes fallback to a baked application.
    pub fn open(opts: &DbOpenOptions) -> Result<Option<Self>> {
        if !opts.read_only {
            super::note_writable_open();
        }
        let empty = std::cell::Cell::new(false);
        let db = Db::open_with_bootstrap(
            opts,
            |conn| {
                let tx = conn.transaction_primary_snapshot()?;
                let version: i64 =
                    tx.query_row("PRAGMA user_version", (), |row| Ok(row.get(0)?))?;
                let objects: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name NOT GLOB 'sqlite_*'",
                    (),
                    |row| Ok(row.get(0)?),
                )?;
                if version == 0 && objects == 0 {
                    empty.set(true);
                } else {
                    anyhow::ensure!(
                        version >= 1
                            && meta_string_conn(&tx, "schema_family")?.as_deref()
                                == Some(SCHEMA_FAMILY),
                        "unsupported update-control database family or version"
                    );
                    // Before control tables exist, only a recognized incomplete
                    // first bootstrap can resume. A completed older installation
                    // is not silently upgraded by the launcher.
                    if version < 15 {
                        anyhow::ensure!(
                            version <= CORE_SCHEMA_VERSION
                                && meta_string_conn(&tx, META_BOOTSTRAP_COMPLETE)?.is_none(),
                            "older installation needs a coordinated-update baseline"
                        );
                        empty.set(true);
                        tx.finish_read()?;
                        return Ok(());
                    }
                    let mut statement = tx.prepare(&format!(
                        "SELECT {} FROM component_updates ORDER BY component",
                        updates::COLS
                    ))?;
                    let rows = statement
                        .query_map((), updates::row_update)?
                        .collect::<Result<Vec<_>>>()?;
                    anyhow::ensure!(
                        rows.iter()
                            .map(|row| row.component.as_str())
                            .eq(["api", "bot", "webui"]),
                        "incomplete update-control rows"
                    );
                    drop(statement);
                    let upgrade = read_upgrade(&tx)?;
                    if version <= CORE_SCHEMA_VERSION
                        && meta_string_conn(&tx, META_BOOTSTRAP_COMPLETE)?.is_none()
                        && upgrade.is_none()
                        && rows.iter().all(|row| {
                            row.installed.is_none()
                                && row.desired.is_none()
                                && row.observed.is_none()
                                && row.installed_launcher.is_none()
                                && row.desired_launcher.is_none()
                        })
                    {
                        empty.set(true);
                    }
                }
                tx.finish_read()?;
                Ok(())
            },
            |_| Ok(()),
        )?;
        Ok((!empty.get()).then_some(Self { db }))
    }

    pub fn from_application(db: &Db) -> Self {
        Self { db: db.clone() }
    }

    pub fn writes_locally(&self) -> bool {
        !self.db.is_read_only()
    }

    pub async fn schema_version(&self) -> Result<i64> {
        core_schema_version(&self.db).await
    }

    pub async fn components(&self) -> Result<Vec<ComponentUpdate>> {
        component_updates(&self.db).await
    }

    pub async fn component(&self, role: &str) -> Result<ComponentUpdate> {
        component_update(&self.db, role).await
    }

    pub async fn upgrade(&self) -> Result<Option<ServiceUpgrade>> {
        self.db
            .with_conn(|conn| {
                let tx = conn.transaction_primary_snapshot()?;
                let upgrade = read_upgrade(&tx)?;
                tx.finish_read()?;
                Ok(upgrade)
            })
            .await
    }

    pub async fn set_channel(&self, role: &str, channel: &str) -> Result<()> {
        set_update_policy(&self.db, role, channel, "off", false)
            .await
            .map(|_| ())
    }

    pub async fn checked(
        &self,
        component: &str,
        channel: &str,
        candidate: Option<ComponentRelease>,
        error: Option<String>,
        now: i64,
    ) -> Result<()> {
        record_update_check(&self.db, component, channel, candidate, error, now)
            .await
            .map(|_| ())
    }

    /// Hold both service check leases before registry work. A failure to take
    /// the second lease releases the first so Administration discovery is not stuck.
    pub async fn acquire_pair_check(&self, now: i64) -> Result<String> {
        let owner = format!("{:032x}", rand::random::<u128>());
        if !claim_update_lease(&self.db, "bot", &owner, now, 240).await? {
            anyhow::bail!("another update operation is running");
        }
        match claim_update_lease(&self.db, "api", &owner, now, 240).await {
            Ok(true) => Ok(owner),
            Ok(false) => {
                release_update_lease(&self.db, "bot", &owner).await?;
                anyhow::bail!("another update operation is running")
            }
            Err(error) => {
                let _ = release_update_lease(&self.db, "bot", &owner).await;
                Err(error)
            }
        }
    }

    pub async fn release_pair_check(&self, owner: &str) -> Result<()> {
        release_update_lease(&self.db, "bot", owner).await?;
        release_update_lease(&self.db, "api", owner).await?;
        Ok(())
    }

    /// Persist channel and checked candidate for bot and API in one transaction.
    /// Either both rows change or the previous selection remains.
    pub async fn record_pair_check(
        &self,
        owner: &str,
        channel: &str,
        bot: ComponentRelease,
        api: ComponentRelease,
        now: i64,
    ) -> Result<()> {
        anyhow::ensure!(
            matches!(channel, "latest" | "stable" | "nightly"),
            "invalid channel"
        );
        anyhow::ensure!(
            owner.len() == 32 && owner.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid update lease"
        );
        bot.validate()?;
        api.validate()?;
        anyhow::ensure!(
            bot.component == "bot"
                && api.component == "api"
                && bot.channel == channel
                && api.channel == channel
                && bot.source_sha == api.source_sha,
            "bot and API must be a matching checked pair"
        );
        let (owner, channel) = (owner.to_owned(), channel.to_owned());
        let bot_json = serde_json::to_string(&bot)?;
        let api_json = serde_json::to_string(&api)?;
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_immediate()?;
                for (component, candidate) in [("bot", bot_json), ("api", api_json)] {
                    let renewed = tx.execute_rows(
                        "UPDATE component_updates SET lease_until=?3 WHERE component=?1 AND lease_owner=?2 AND lease_until>?4",
                        params![component, owner.clone(), now + 240, now],
                        1,
                    )?;
                    anyhow::ensure!(renewed == 1, "update lease expired or changed");
                    let channel_rows = tx.execute_rows(
                        "UPDATE component_updates SET channel=?2,cadence='off',candidate=CASE WHEN channel=?2 THEN candidate ELSE NULL END,checked_at=CASE WHEN channel=?2 THEN checked_at ELSE NULL END,check_error=CASE WHEN channel=?2 THEN check_error ELSE NULL END WHERE component=?1",
                        params![component, channel.clone()],
                        1,
                    )?;
                    anyhow::ensure!(channel_rows == 1, "paired check could not select the channel");
                    let checked = tx.execute_rows(
                        "UPDATE component_updates SET candidate=?2,checked_at=?3,check_error=NULL WHERE component=?1 AND channel=?4",
                        params![component, candidate, now, channel.clone()],
                        1,
                    )?;
                    anyhow::ensure!(checked == 1, "release channel changed during check; check again");
                }
                tx.commit()?;
                Ok(())
            })
            .await
    }

    pub async fn running(
        &self,
        role: &str,
        release: Option<ComponentRelease>,
        observation: ComponentObservation,
        now: i64,
    ) -> Result<()> {
        record_component_running(&self.db, role, release, observation, now).await
    }

    pub async fn baked_running(
        &self,
        role: &str,
        observation: ComponentObservation,
        now: i64,
    ) -> Result<()> {
        record_baked_component_running(&self.db, role, observation, now).await
    }

    pub async fn failed(&self, role: &str, release: &ComponentRelease, error: &str) -> Result<()> {
        record_component_failure(&self.db, role, release, error).await
    }

    pub async fn rolled_back(
        &self,
        role: &str,
        failed: &ComponentRelease,
        previous: Option<ComponentRelease>,
        observation: ComponentObservation,
        error: &str,
        now: i64,
    ) -> Result<()> {
        record_component_rollback(&self.db, role, failed, previous, observation, error, now).await
    }

    /// Only the baked controller can establish the baked application's schema.
    pub async fn baked_compatibility(
        &self,
        role: &str,
        base_sha: &str,
        baked_controller: bool,
    ) -> Result<BakedCompatibility> {
        anyhow::ensure!(matches!(role, "bot" | "api"), "invalid launcher role");
        let key = format!("component_baked_compatibility_{role}_v1");
        let base_sha = base_sha.to_owned();
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_immediate()?;
                let result = if baked_controller {
                    let baked = BakedCompatibility::compiled(&base_sha);
                    write_meta_string_conn(&tx, &key, &serde_json::to_string(&baked)?)?;
                    baked
                } else {
                    let raw = meta_string_conn(&tx, &key)?.context(
                        "baked compatibility missing; restart the container's baked controller",
                    )?;
                    let baked: BakedCompatibility = serde_json::from_str(&raw)?;
                    anyhow::ensure!(
                        baked.source_sha == base_sha && baked.core >= 15 && baked.guild >= 1,
                        "baked compatibility does not match this base image"
                    );
                    baked
                };
                tx.commit()?;
                Ok(result)
            })
            .await
    }

    /// Refresh actual controller capability without an application heartbeat.
    pub async fn observe_launcher(&self, role: &str, revision: u32) -> Result<()> {
        anyhow::ensure!(matches!(role, "bot" | "api"), "invalid launcher role");
        let role = role.to_owned();
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_immediate()?;
                let raw: Option<String> = tx.query_row(
                    "SELECT observed FROM component_updates WHERE component=?1",
                    params![role.clone()],
                    |row| Ok(row.get(0)?),
                )?;
                if let Some(raw) = raw {
                    let mut seen: ComponentObservation = serde_json::from_str(&raw)?;
                    seen.launcher_revision = revision;
                    // Preserve pre-protocol baseline adoption until the baked app
                    // reports readiness; a legacy schema15 row may describe DB16.
                    if seen.update_protocol != 0 {
                        seen.update_protocol = UPDATE_CONTROL_PROTOCOL;
                    }
                    tx.execute_rows(
                        "UPDATE component_updates SET observed=?2 WHERE component=?1",
                        params![role, serde_json::to_string(&seen)?],
                        1,
                    )?;
                }
                tx.commit()
            })
            .await
    }

    pub async fn clear_superseded_launcher(
        &self,
        role: &str,
        release: &ComponentRelease,
    ) -> Result<()> {
        let (role, release) = (role.to_owned(), serde_json::to_string(release)?);
        self.db.with_conn(move |conn| {
            let tx = conn.transaction_immediate()?;
            tx.execute_rows("UPDATE component_updates SET desired_launcher=NULL WHERE component=?1 AND desired_launcher=?2",
                params![role, release], 1)?;
            tx.commit()
        }).await
    }

    /// Clear a stored launcher selection that has no capability contract.
    /// Compare the parsed release, then swap on the original stored text.
    /// Rows saved before launcher labels omit fields this serializer writes.
    pub async fn clear_uncontracted_launcher(
        &self,
        role: &str,
        release: &ComponentRelease,
    ) -> Result<()> {
        anyhow::ensure!(release.launcher.is_none(), "launcher contract is present");
        let (role, release) = (role.to_owned(), release.clone());
        self.db.with_conn(move |conn| {
            let tx = conn.transaction_immediate()?;
            let stored: Option<String> = tx.query_row(
                "SELECT desired_launcher FROM component_updates WHERE component=?1",
                params![role.clone()],
                |row| Ok(row.get(0)?),
            )?;
            let stored = stored.context("desired launcher is already clear")?;
            let parsed: ComponentRelease = serde_json::from_str(&stored)?;
            anyhow::ensure!(
                parsed.launcher.is_none() && parsed == release,
                "desired launcher changed before clear"
            );
            tx.execute_rows(
                "UPDATE component_updates SET desired_launcher=NULL WHERE component=?1 AND desired_launcher=?2",
                params![role, stored],
                1,
            )?;
            tx.commit()
        }).await
    }

    pub async fn launcher_running(
        &self,
        role: &str,
        release: &ComponentRelease,
        hash: &str,
        now: i64,
    ) -> Result<()> {
        record_launcher_running(&self.db, role, release, hash, now).await
    }

    pub async fn baked_launcher(&self, role: &str, hash: &str, now: i64) -> Result<()> {
        record_baked_launcher(&self.db, role, hash, now).await
    }

    pub async fn launcher_failed(&self, role: &str, digest: &str, error: &str) -> Result<()> {
        record_launcher_failure(&self.db, role, digest, error).await
    }

    /// Pin both checked targets atomically before either service is interrupted.
    /// Recovery is an explicit owner action and may only move forwards.
    pub async fn request_upgrade(
        &self,
        bot_digest: &str,
        api_digest: &str,
        recover: bool,
        now: i64,
    ) -> Result<ServiceUpgrade> {
        let (bot_digest, api_digest) = (bot_digest.to_owned(), api_digest.to_owned());
        self.db.with_conn(move |conn| {
            let tx = conn.transaction_immediate()?;
            let existing = read_upgrade(&tx)?;
            if let Some(active) = existing.as_ref().filter(|value| value.phase.active()) {
                if active.bot.release.digest == bot_digest && active.api.release.digest == api_digest && (!recover || active.error.is_none()) {
                    tx.commit()?;
                    return Ok(active.clone());
                }
                anyhow::ensure!(recover && active.error.is_some() && active.bot.stopped && active.api.stopped, "wait until both services have stopped before recovering the upgrade");
            }
            let bot = tx.query_row(&format!("SELECT {} FROM component_updates WHERE component='bot'", updates::COLS), (), updates::row_update)?;
            let api = tx.query_row(&format!("SELECT {} FROM component_updates WHERE component='api'", updates::COLS), (), updates::row_update)?;
            let retry = existing.as_ref().filter(|active| active.phase.active() && recover
                && active.bot.release.digest == bot_digest && active.api.release.digest == api_digest);
            let (target_bot, target_api) = if let Some(active) = retry {
                (active.bot.release.clone(), active.api.release.clone())
            } else {
            let target_bot = updates::checked_install_candidate(bot.candidate.as_ref().map(serde_json::to_string).transpose()?, &bot.channel, bot.checked_at, bot.check_error.clone(), &bot_digest, now)?;
            let target_api = updates::checked_install_candidate(api.candidate.as_ref().map(serde_json::to_string).transpose()?, &api.channel, api.checked_at, api.check_error.clone(), &api_digest, now)?;
                (target_bot, target_api)
            };
            target_bot.validate()?;
            target_api.validate()?;
            let bot_contract = target_bot.upgrade.as_ref().context("bot release does not support coordinated upgrades")?;
            let api_contract = target_api.upgrade.as_ref().context("API release does not support coordinated upgrades")?;
            anyhow::ensure!(bot_contract.control_protocol == UPDATE_CONTROL_PROTOCOL && target_bot.launcher.is_some() && target_api.launcher.is_some(),
                "release lacks launcher-aware upgrade requirements");
            anyhow::ensure!(target_bot.component == "bot" && target_api.component == "api"
                && target_bot.source_sha == target_api.source_sha
                && target_bot.result_schema == target_api.result_schema
                && bot_contract == api_contract
                && target_bot.min_schema == target_bot.result_schema && target_bot.max_schema == target_bot.result_schema
                && target_api.min_schema == target_api.result_schema && target_api.max_schema == target_api.result_schema,
                "bot and API must come from the same compatible source build");
            let schema: i64 = tx.query_row("PRAGMA user_version", (), |row| Ok(row.get(0)?))?;
            let recovering = existing.as_ref().filter(|value| value.phase.active() && recover);
            let irreversible = recovering.is_some_and(|value| value.irreversible);
            let from_guild = if let Some(previous) = recovering {
                previous.bot.release.upgrade.as_ref().context("missing prior upgrade contract")?.guild_schema.max(previous.from_guild)
            } else {
                let bot_seen = bot.observed.as_ref().context("bot has not been observed")?;
                let api_seen = api.observed.as_ref().context("API has not been observed")?;
                anyhow::ensure!(bot_seen.guild_schema.is_some() && bot_seen.guild_schema == api_seen.guild_schema,
                    "current guild schema compatibility cannot be verified");
                bot_seen.guild_schema.context("current guild schema unknown")?
            };
            anyhow::ensure!(schema >= bot_contract.min_core && schema <= target_bot.result_schema
                && from_guild >= bot_contract.min_guild && from_guild <= bot_contract.guild_schema,
                "release cannot upgrade the current databases; downgrades are not allowed");
            for (state, target) in [(&bot, &target_bot), (&api, &target_api)] {
                let observed = state.observed.as_ref().context("service has not been observed")?;
                anyhow::ensure!(observed.update_protocol >= UPDATE_CONTROL_PROTOCOL && observed.self_update_enabled
                    && observed.base_abi == target.base_abi, "service needs the coordinated-update baseline enabled");
                if recovering.is_none() {
                    // Explicit owner recovery may run while no application can
                    // refresh its heartbeat. Both live controllers still have to
                    // stage/validate and acknowledge before anything can migrate.
                    anyhow::ensure!(recover || state.observed_at.is_some_and(|seen| seen <= now && now - seen < 7200), "service observation is stale");
                    anyhow::ensure!(state.desired.as_ref().is_none_or(|desired| state.installed.as_ref().is_some_and(|installed| installed.digest == desired.digest))
                        && state.desired_launcher.as_ref().is_none_or(|desired| state.installed_launcher.as_ref().is_some_and(|installed| installed.digest == desired.digest)),
                        "an independent update is already pending");
                }
            }
            let service = |release| UpgradeService { release, instance: String::new(), prepared: false, stopped: false, ready: false };
            let upgrade = ServiceUpgrade {
                id: format!("{:032x}", rand::random::<u128>()), phase: UpgradePhase::Preparing,
                bot: service(target_bot), api: service(target_api), from_core: schema, from_guild,
                irreversible, error: None,
            };
            write_upgrade(&tx, &upgrade)?;
            for target in [&upgrade.bot.release, &upgrade.api.release] {
                tx.execute_rows("UPDATE component_updates SET desired=?2,state='queued',last_error=NULL WHERE component=?1",
                    params![target.component.clone(), serde_json::to_string(target)?], 1)?;
            }
            tx.commit()?;
            Ok(upgrade)
        }).await
    }

    /// Pin a required launcher only while the owning application selection is
    /// still current. Preparing is reused across the launcher re-exec; the new
    /// incarnation must prepare again before any schema-changing application.
    pub async fn select_required_launcher(
        &self,
        release: &ComponentRelease,
        operation: Option<&str>,
    ) -> Result<()> {
        release.validate()?;
        let release = release.clone();
        let operation = operation.map(str::to_owned);
        self.db.with_conn(move |conn| {
            let tx = conn.transaction_immediate()?;
            if let Some(id) = operation {
                let op = read_upgrade(&tx)?.context("upgrade missing")?;
                require_progress(op.id == id && op.phase == UpgradePhase::Preparing && op.error.is_none()
                    && op.service(&release.component)?.release == release)?;
            } else {
                ensure_no_service_upgrade(&tx)?;
                let row = tx.query_row(&format!("SELECT {} FROM component_updates WHERE component=?1", updates::COLS), params![release.component.clone()], updates::row_update)?;
                anyhow::ensure!(row.desired.as_ref() == Some(&release) || row.installed.as_ref() == Some(&release),
                    "application selection changed before launcher handoff");
            }
            tx.execute_rows("UPDATE component_updates SET desired_launcher=?2,last_error=NULL WHERE component=?1",
                params![release.component.clone(),serde_json::to_string(&release)?], 1)?;
            tx.commit()
        }).await
    }

    pub async fn abort_upgrade(&self, id: &str) -> Result<()> {
        let id = id.to_owned();
        self.db.with_conn(move |conn| {
            let tx = conn.transaction_immediate()?;
            let mut upgrade = read_upgrade(&tx)?.context("no coordinated upgrade")?;
            anyhow::ensure!(upgrade.id == id && upgrade.phase.active() && !upgrade.irreversible,
                "migration may have started; select a forward recovery release instead");
            upgrade.phase = UpgradePhase::Aborted;
            write_upgrade(&tx, &upgrade)?;
            tx.execute_rows("UPDATE component_updates SET desired=installed,state='running',last_error=NULL WHERE component IN ('bot','api')", (), 2)?;
            tx.commit()
        }).await
    }

    /// Invalidate staging/stop acknowledgements from a previous process lifetime.
    pub async fn join_upgrade(
        &self,
        id: &str,
        role: &str,
        instance: &str,
    ) -> Result<ServiceUpgrade> {
        let (id, role, instance) = (id.to_owned(), role.to_owned(), instance.to_owned());
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_immediate()?;
                let mut upgrade = read_upgrade(&tx)?.context("no coordinated upgrade")?;
                require_progress(upgrade.id == id && upgrade.phase.active())?;
                let service = upgrade.service_mut(&role)?;
                if service.instance != instance {
                    service.instance = instance;
                    service.prepared = false;
                    service.stopped = false;
                    service.ready = false;
                    // A restarted launcher has lost its local staging and child
                    // evidence. Re-quiesce both roles; retain the forward-only fence.
                    upgrade.phase = UpgradePhase::Preparing;
                    upgrade.bot.ready = false;
                    upgrade.api.ready = false;
                    upgrade.bot.stopped = false;
                    upgrade.api.stopped = false;
                    write_upgrade(&tx, &upgrade)?;
                }
                tx.commit()?;
                Ok(upgrade)
            })
            .await
    }

    /// Called only after the real action succeeds; elapsed time is never proof
    /// that a child stopped. Every write checks the operation and incarnation.
    pub async fn acknowledge(
        &self,
        id: &str,
        role: &str,
        instance: &str,
        action: UpgradeAcknowledgement,
    ) -> Result<ServiceUpgrade> {
        let (id, role, instance) = (id.to_owned(), role.to_owned(), instance.to_owned());
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_immediate()?;
                let mut upgrade = read_upgrade(&tx)?.context("no coordinated upgrade")?;
                require_progress(upgrade.id == id && upgrade.phase.active())?;
                require_progress(upgrade.service(&role)?.instance == instance)?;
                match action {
                    UpgradeAcknowledgement::Prepared => upgrade.service_mut(&role)?.prepared = true,
                    UpgradeAcknowledgement::Stopped => {
                        require_progress(
                            upgrade.error.is_some()
                                || (role == "api" && upgrade.phase != UpgradePhase::Preparing)
                                || (role == "bot"
                                    && !matches!(
                                        upgrade.phase,
                                        UpgradePhase::Preparing | UpgradePhase::StoppingApi
                                    )),
                        )?;
                        upgrade.service_mut(&role)?.stopped = true;
                        upgrade.service_mut(&role)?.ready = false;
                    }
                    UpgradeAcknowledgement::Starting => {
                        require_progress(
                            upgrade.error.is_none()
                                && upgrade.service(&role)?.prepared
                                && ((role == "bot"
                                    && upgrade.phase.forward_only()
                                    && upgrade.api.stopped)
                                    || (role == "api"
                                        && upgrade.phase == UpgradePhase::StartingApi
                                        && upgrade.bot.ready)),
                        )?;
                        upgrade.service_mut(&role)?.stopped = false;
                        upgrade.irreversible = true;
                    }
                    UpgradeAcknowledgement::Ready => {
                        require_progress(upgrade.error.is_none())?;
                        require_progress(
                            (role == "bot" && upgrade.phase.forward_only())
                                || (role == "api" && upgrade.phase == UpgradePhase::StartingApi),
                        )?;
                        upgrade.service_mut(&role)?.ready = true;
                    }
                }
                upgrade.phase = match upgrade.phase {
                    phase if upgrade.error.is_some() => phase,
                    UpgradePhase::Preparing if upgrade.bot.prepared && upgrade.api.prepared => {
                        UpgradePhase::StoppingApi
                    }
                    UpgradePhase::StoppingApi if upgrade.api.stopped && upgrade.bot.prepared => {
                        UpgradePhase::StoppingBot
                    }
                    UpgradePhase::StoppingBot
                        if upgrade.api.stopped
                            && upgrade.bot.stopped
                            && upgrade.api.prepared
                            && upgrade.bot.prepared =>
                    {
                        UpgradePhase::StartingBot
                    }
                    UpgradePhase::StartingBot
                        if upgrade.bot.ready && upgrade.api.stopped && upgrade.api.prepared =>
                    {
                        UpgradePhase::StartingApi
                    }
                    UpgradePhase::StartingApi if upgrade.bot.ready && upgrade.api.ready => {
                        UpgradePhase::Complete
                    }
                    phase => phase,
                };
                upgrade.irreversible |= upgrade.phase.forward_only();
                write_upgrade(&tx, &upgrade)?;
                tx.commit()?;
                Ok(upgrade)
            })
            .await
    }

    pub async fn upgrade_error(&self, id: &str, error: &str) -> Result<()> {
        let (id, error) = (id.to_owned(), error.chars().take(300).collect::<String>());
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_immediate()?;
                let mut upgrade = read_upgrade(&tx)?.context("no coordinated upgrade")?;
                anyhow::ensure!(
                    upgrade.id == id && upgrade.phase != UpgradePhase::Aborted,
                    "upgrade changed"
                );
                if upgrade.phase == UpgradePhase::Complete {
                    upgrade.phase = UpgradePhase::StartingBot;
                }
                upgrade.error = Some(error);
                write_upgrade(&tx, &upgrade)?;
                tx.commit()
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        control: UpdateControl,
        path: PathBuf,
        opts: DbOpenOptions,
    }
    impl Fixture {
        async fn new(core: i64, guild: i64) -> Self {
            let path = std::env::temp_dir()
                .join(format!("paired-control-{:032x}.db", rand::random::<u128>()));
            let db = Db::open(&path, 2).unwrap();
            let opts = DbOpenOptions {
                read_only: false,
                path: path.clone(),
                url: String::new(),
                token: String::new(),
                pool_size: 2,
                migrate: false,
                default_prefix: "!".into(),
                retention_days: 1,
            };
            let control = UpdateControl::open(&opts).unwrap().unwrap();
            for (role, digest) in [("bot", 'a'), ("api", 'b')] {
                let release = ComponentRelease {
                    component: role.into(),
                    channel: "latest".into(),
                    release_id: "paired-test".into(),
                    source_sha: "c".repeat(40),
                    published_at: "2026-10-01T00:00:00Z".into(),
                    image: format!("ghcr.io/jatmn/jmodguard-{role}"),
                    digest: format!("sha256:{}", digest.to_string().repeat(64)),
                    min_schema: core,
                    max_schema: core,
                    result_schema: core,
                    min_api: 1,
                    max_api: 1,
                    launcher_protocol: 1,
                    launcher: Some(crate::db::LauncherCompatibility {
                        revision: crate::db::LAUNCHER_REVISION,
                        min_revision: crate::db::REQUIRED_LAUNCHER_REVISION,
                    }),
                    base_abi: "debian-bookworm-amd64".into(),
                    upgrade: Some(UpgradeCompatibility {
                        control_protocol: UPDATE_CONTROL_PROTOCOL,
                        min_core: 15,
                        min_guild: 1,
                        guild_schema: guild,
                    }),
                };
                control
                    .checked(role, "latest", Some(release), None, 100)
                    .await
                    .unwrap();
                control
                    .running(
                        role,
                        None,
                        ComponentObservation {
                            source_sha: "d".repeat(40),
                            executable_sha256: "e".repeat(64),
                            admin_sha256: None,
                            base_image_sha: "d".repeat(40),
                            started_at: 90,
                            min_schema: CORE_SCHEMA_VERSION,
                            max_schema: CORE_SCHEMA_VERSION,
                            api_capability: 1,
                            self_update_enabled: true,
                            base_abi: "debian-bookworm-amd64".into(),
                            guild_schema: Some(guild_data::SCHEMA_VERSION),
                            update_protocol: UPDATE_CONTROL_PROTOCOL,
                            launcher_revision: 0,
                        },
                        100,
                    )
                    .await
                    .unwrap();
            }
            drop(db);
            Self {
                control,
                path,
                opts,
            }
        }
        async fn queue(&self, recover: bool, now: i64) -> Result<ServiceUpgrade> {
            self.control
                .request_upgrade(
                    &format!("sha256:{}", "a".repeat(64)),
                    &format!("sha256:{}", "b".repeat(64)),
                    recover,
                    now,
                )
                .await
        }
        async fn ack(
            &self,
            id: &str,
            role: &str,
            action: UpgradeAcknowledgement,
        ) -> ServiceUpgrade {
            self.control
                .acknowledge(id, role, role, action)
                .await
                .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
            }
        }
    }

    #[tokio::test]
    async fn pair_check_commits_both_services_or_leaves_the_previous_selection() {
        let f = Fixture::new(CORE_SCHEMA_VERSION, guild_data::SCHEMA_VERSION).await;
        let before_bot = f.control.component("bot").await.unwrap();
        let before_api = f.control.component("api").await.unwrap();
        let held = "ab".repeat(16);
        assert!(
            claim_update_lease(&f.control.db, "api", &held, 100, 240)
                .await
                .unwrap()
        );
        assert!(f.control.acquire_pair_check(100).await.is_err());
        assert_eq!(
            f.control
                .component("bot")
                .await
                .unwrap()
                .candidate
                .unwrap()
                .digest,
            before_bot.candidate.as_ref().unwrap().digest
        );
        release_update_lease(&f.control.db, "api", &held)
            .await
            .unwrap();
        assert!(
            claim_update_lease(&f.control.db, "bot", &"ef".repeat(16), 100, 60)
                .await
                .unwrap(),
            "a failed pair-check admission must not keep the bot lease"
        );
        release_update_lease(&f.control.db, "bot", &"ef".repeat(16))
            .await
            .unwrap();

        let owner = f.control.acquire_pair_check(100).await.unwrap();
        let mut bot = before_bot.candidate.clone().unwrap();
        let mut api = before_api.candidate.clone().unwrap();
        bot.digest = format!("sha256:{}", "1".repeat(64));
        api.digest = format!("sha256:{}", "2".repeat(64));
        api.source_sha = "f".repeat(40);
        assert!(
            f.control
                .record_pair_check(&owner, "latest", bot.clone(), api.clone(), 100)
                .await
                .is_err()
        );
        assert_eq!(
            f.control
                .component("bot")
                .await
                .unwrap()
                .candidate
                .unwrap()
                .digest,
            before_bot.candidate.as_ref().unwrap().digest
        );
        api.source_sha = bot.source_sha.clone();
        f.control
            .db
            .with_conn(|conn| {
                conn.execute_rows(
                    "UPDATE component_updates SET lease_owner=?1 WHERE component='api'",
                    params!["cd".repeat(16)],
                    1,
                )?;
                Ok(())
            })
            .await
            .unwrap();
        bot.channel = "stable".into();
        api.channel = "stable".into();
        assert!(
            f.control
                .record_pair_check(&owner, "stable", bot.clone(), api.clone(), 100)
                .await
                .is_err()
        );
        assert_eq!(f.control.component("bot").await.unwrap().channel, "latest");
        assert_eq!(
            f.control
                .component("bot")
                .await
                .unwrap()
                .candidate
                .unwrap()
                .digest,
            before_bot.candidate.as_ref().unwrap().digest
        );
        let lease_owner = owner.clone();
        f.control
            .db
            .with_conn(move |conn| {
                conn.execute_rows(
                    "UPDATE component_updates SET lease_owner=?1,lease_until=?2 WHERE component='api'",
                    params![lease_owner, 1_000_i64],
                    1,
                )?;
                Ok(())
            })
            .await
            .unwrap();
        f.control
            .record_pair_check(&owner, "stable", bot.clone(), api.clone(), 100)
            .await
            .unwrap();
        let saved_bot = f.control.component("bot").await.unwrap();
        let saved_api = f.control.component("api").await.unwrap();
        assert_eq!(saved_bot.channel, "stable");
        assert_eq!(saved_api.channel, "stable");
        assert_eq!(saved_bot.cadence, "off");
        assert_eq!(saved_api.cadence, "off");
        assert_eq!(saved_bot.candidate.unwrap().digest, bot.digest);
        assert_eq!(saved_api.candidate.unwrap().digest, api.digest);
        f.control.release_pair_check(&owner).await.unwrap();
    }

    #[tokio::test]
    async fn pair_pins_both_candidates_and_fences_irreversible_recovery() {
        let f = Fixture::new(CORE_SCHEMA_VERSION + 1, guild_data::SCHEMA_VERSION + 1).await;
        assert!(f.queue(false, 7400).await.is_err());
        let op = f.queue(false, 100).await.unwrap();
        assert_eq!(op.phase, UpgradePhase::Preparing);
        assert!(
            request_component_install(&f.control.db, "api", &op.api.release.digest, 100)
                .await
                .is_err()
        );
        assert!(
            request_launcher_install(&f.control.db, "bot", &op.bot.release.digest, 100)
                .await
                .is_err()
        );
        for role in ["bot", "api"] {
            f.control.join_upgrade(&op.id, role, role).await.unwrap();
        }
        assert!(
            f.control
                .acknowledge(&op.id, "bot", "bot", UpgradeAcknowledgement::Starting)
                .await
                .is_err()
        );
        assert_eq!(
            f.ack(&op.id, "bot", UpgradeAcknowledgement::Prepared)
                .await
                .phase,
            UpgradePhase::Preparing
        );
        assert_eq!(
            f.ack(&op.id, "api", UpgradeAcknowledgement::Prepared)
                .await
                .phase,
            UpgradePhase::StoppingApi
        );
        assert!(
            f.control
                .acknowledge(&op.id, "bot", "bot", UpgradeAcknowledgement::Stopped)
                .await
                .is_err()
        );
        assert_eq!(
            f.ack(&op.id, "api", UpgradeAcknowledgement::Stopped)
                .await
                .phase,
            UpgradePhase::StoppingBot
        );
        let fence = f.ack(&op.id, "bot", UpgradeAcknowledgement::Stopped).await;
        assert_eq!(fence.phase, UpgradePhase::StartingBot);
        assert!(fence.irreversible);
        assert!(f.control.abort_upgrade(&op.id).await.is_err());
        f.ack(&op.id, "bot", UpgradeAcknowledgement::Starting).await;
        f.control
            .upgrade_error(&op.id, "bot startup failed after guild migration")
            .await
            .unwrap();
        // Neither a missing heartbeat nor a prior stop acknowledgement proves
        // the newly launched process is gone. Reap/ack is required to replace it.
        assert!(f.queue(true, 100).await.is_err());
        f.ack(&op.id, "bot", UpgradeAcknowledgement::Stopped).await;
        assert!(f.control.upgrade().await.unwrap().unwrap().error.is_some());
        let retry = f.queue(true, 8000).await.unwrap(); // pinned retry survives stale discovery
        assert_ne!(retry.id, op.id);
        assert!(retry.irreversible);
        assert!(f.control.abort_upgrade(&retry.id).await.is_err());
        assert!(
            f.control
                .acknowledge(&op.id, "api", "api", UpgradeAcknowledgement::Ready)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn restart_invalidates_local_proof_and_control_survives_newer_app_schema() {
        let f = Fixture::new(CORE_SCHEMA_VERSION + 1, guild_data::SCHEMA_VERSION).await;
        let op = f.queue(false, 100).await.unwrap();
        for role in ["bot", "api"] {
            f.control.join_upgrade(&op.id, role, role).await.unwrap();
            f.ack(&op.id, role, UpgradeAcknowledgement::Prepared).await;
        }
        f.ack(&op.id, "api", UpgradeAcknowledgement::Stopped).await;
        f.ack(&op.id, "bot", UpgradeAcknowledgement::Stopped).await;
        f.ack(&op.id, "bot", UpgradeAcknowledgement::Starting).await;
        // Stand in for the new app's migration; the control handle itself must
        // neither run migrations nor make the ordinary old Db accept this.
        f.control
            .db
            .with_conn(|conn| {
                conn.pragma_update(None, "user_version", CORE_SCHEMA_VERSION + 1)?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(Db::open_with(f.opts.clone()).is_err());
        let reopened = UpdateControl::open(&f.opts).unwrap().unwrap();
        assert_eq!(
            reopened.schema_version().await.unwrap(),
            CORE_SCHEMA_VERSION + 1
        );
        let resumed = reopened
            .join_upgrade(&op.id, "bot", "new-process")
            .await
            .unwrap();
        assert_eq!(resumed.phase, UpgradePhase::Preparing);
        assert!(resumed.irreversible);
        assert!(!resumed.bot.prepared && !resumed.bot.stopped && !resumed.api.stopped);
        assert!(
            reopened
                .acknowledge(&op.id, "bot", "bot", UpgradeAcknowledgement::Ready)
                .await
                .is_err()
        );
        reopened
            .upgrade_error(&op.id, "retry after restart")
            .await
            .unwrap();
        assert!(Db::open_with(f.opts.clone()).is_err());
    }

    #[tokio::test]
    async fn source_mismatch_and_downgrades_are_rejected_without_queuing() {
        let f = Fixture::new(CORE_SCHEMA_VERSION, guild_data::SCHEMA_VERSION + 1).await;
        let mut api = f.control.component("api").await.unwrap().candidate.unwrap();
        api.source_sha = "f".repeat(40);
        f.control
            .checked("api", "latest", Some(api.clone()), None, 100)
            .await
            .unwrap();
        assert!(f.queue(false, 100).await.is_err());
        assert!(f.control.upgrade().await.unwrap().is_none());
        assert!(f.control.component("bot").await.unwrap().desired.is_none());
        api.source_sha = "c".repeat(40);
        api.upgrade.as_mut().unwrap().guild_schema = guild_data::SCHEMA_VERSION - 1;
        f.control
            .checked("api", "latest", Some(api), None, 100)
            .await
            .unwrap();
        assert!(f.queue(false, 100).await.is_err());
    }

    #[tokio::test]
    async fn stopped_application_can_admit_explicit_owner_recovery_without_fresh_heartbeat() {
        let f = Fixture::new(CORE_SCHEMA_VERSION + 1, guild_data::SCHEMA_VERSION).await;
        for role in ["bot", "api"] {
            let release = f.control.component(role).await.unwrap().candidate.unwrap();
            f.control
                .checked(role, "latest", Some(release), None, 8000)
                .await
                .unwrap();
        }
        assert!(f.queue(false, 8000).await.is_err());
        assert_eq!(
            f.queue(true, 8000).await.unwrap().phase,
            UpgradePhase::Preparing
        );
    }

    #[tokio::test]
    async fn interrupted_first_boot_resumes_without_adopting_existing_installations() {
        for version in [1usize, 14, 15] {
            let path = std::env::temp_dir().join(format!(
                "initial-control-{:032x}.db",
                rand::random::<u128>()
            ));
            let opts = DbOpenOptions {
                read_only: false,
                path: path.clone(),
                url: String::new(),
                token: String::new(),
                pool_size: 1,
                migrate: false,
                default_prefix: "!".into(),
                retention_days: 1,
            };
            let partial = Db::open_with_bootstrap(
                &opts,
                |conn| {
                    for (index, migration) in MIGRATIONS.iter().enumerate().take(version) {
                        let tx = conn.transaction_immediate()?;
                        tx.execute_batch_bounded(migration, 16 * 1024 * 1024)?;
                        tx.pragma_update(None, "user_version", index + 1)?;
                        tx.commit()?;
                    }
                    Ok(())
                },
                |_| Ok(()),
            )
            .unwrap();
            drop(partial);
            assert!(UpdateControl::open(&opts).unwrap().is_none());
            assert!(Db::open_with(opts.clone()).is_err());
            drop(Db::open(&path, 1).unwrap());
            assert!(UpdateControl::open(&opts).unwrap().is_some());
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
            }
        }
    }

    #[tokio::test]
    async fn empty_and_foreign_stores_do_not_get_application_bootstrap_writes() {
        let f = Fixture::new(CORE_SCHEMA_VERSION, guild_data::SCHEMA_VERSION).await;
        f.control
            .db
            .with_conn(|conn| {
                conn.execute_rows(
                    "UPDATE meta SET value='foreign-family' WHERE key='schema_family'",
                    (),
                    1,
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(UpdateControl::open(&f.opts).is_err());
        let mut empty = f.opts.clone();
        empty.path = f.path.with_extension("empty");
        assert!(UpdateControl::open(&empty).unwrap().is_none());
        let _ = std::fs::remove_file(empty.path);
    }
}
