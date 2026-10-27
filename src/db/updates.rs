//! Durable release discovery and installation coordination for the three
//! independently deployed components. No Bunny account credential is stored.
use super::*;
use serde::Deserialize;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LauncherCompatibility {
    pub revision: u32,
    pub min_revision: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ComponentRelease {
    pub component: String,
    pub channel: String,
    pub release_id: String,
    pub source_sha: String,
    pub published_at: String,
    pub image: String,
    pub digest: String,
    pub min_schema: i64,
    pub max_schema: i64,
    pub result_schema: i64,
    pub min_api: i64,
    pub max_api: i64,
    pub launcher_protocol: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launcher: Option<LauncherCompatibility>,
    pub base_abi: String,
    #[serde(default)]
    pub upgrade: Option<UpgradeCompatibility>,
}

impl ComponentRelease {
    pub fn required_launcher(&self) -> u32 {
        self.launcher.as_ref().map_or(0, |v| v.min_revision)
    }

    pub fn supports_launcher(&self, revision: u32) -> Result<()> {
        self.validate()?;
        anyhow::ensure!(
            revision >= self.required_launcher(),
            "launcher update required before application start (need {}, running {})",
            self.required_launcher(),
            revision
        );
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        validate_component(&self.component)?;
        anyhow::ensure!(
            matches!(self.channel.as_str(), "latest" | "stable" | "nightly"),
            "invalid channel"
        );
        anyhow::ensure!(
            self.source_sha.len() == 40 && self.source_sha.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid source SHA"
        );
        anyhow::ensure!(
            self.digest.starts_with("sha256:")
                && self.digest.len() == 71
                && self.digest[7..].bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid image digest"
        );
        anyhow::ensure!(
            self.min_schema <= self.max_schema && self.min_api <= self.max_api,
            "invalid compatibility range"
        );
        anyhow::ensure!(
            self.result_schema >= self.min_schema && self.result_schema <= self.max_schema,
            "resulting schema is outside candidate range"
        );
        anyhow::ensure!(self.launcher_protocol == 1, "unsupported launcher protocol");
        if let Some(upgrade) = &self.upgrade {
            anyhow::ensure!(
                matches!(upgrade.control_protocol, 2 | UPDATE_CONTROL_PROTOCOL)
                    && upgrade.min_core >= 15
                    && upgrade.min_core <= self.result_schema
                    && upgrade.min_guild >= 1
                    && upgrade.min_guild <= upgrade.guild_schema,
                "unsupported schema upgrade contract"
            );
        }
        if let Some(launcher) = &self.launcher {
            anyhow::ensure!(
                launcher.min_revision > 0 && launcher.revision >= launcher.min_revision,
                "invalid launcher requirement"
            );
        }
        if self.component != "webui"
            && self
                .upgrade
                .as_ref()
                .is_some_and(|v| v.control_protocol == UPDATE_CONTROL_PROTOCOL)
        {
            anyhow::ensure!(
                self.launcher.is_some(),
                "release lacks launcher requirements"
            );
        }
        anyhow::ensure!(
            matches!(
                self.base_abi.as_str(),
                "debian-bookworm-amd64" | "static-webui-v1"
            ),
            "unsupported base ABI"
        );
        anyhow::ensure!(
            (self.component == "webui" && self.base_abi == "static-webui-v1")
                || (self.component != "webui" && self.base_abi == "debian-bookworm-amd64"),
            "base ABI does not match component"
        );
        anyhow::ensure!(
            self.release_id.len() <= 128 && !self.release_id.is_empty(),
            "invalid release ID"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentObservation {
    pub source_sha: String,
    pub executable_sha256: String,
    #[serde(default)]
    pub admin_sha256: Option<String>,
    pub base_image_sha: String,
    pub started_at: i64,
    pub min_schema: i64,
    pub max_schema: i64,
    pub api_capability: i64,
    pub self_update_enabled: bool,
    #[serde(default)]
    pub base_abi: String,
    #[serde(default)]
    pub guild_schema: Option<i64>,
    #[serde(default)]
    pub update_protocol: u32,
    #[serde(default)]
    pub launcher_revision: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentUpdate {
    pub component: String,
    pub channel: String,
    pub cadence: String,
    pub candidate: Option<ComponentRelease>,
    pub checked_at: Option<i64>,
    pub check_error: Option<String>,
    pub desired: Option<ComponentRelease>,
    pub desired_launcher: Option<ComponentRelease>,
    pub installed: Option<ComponentRelease>,
    pub installed_launcher: Option<ComponentRelease>,
    pub launcher_sha256: Option<String>,
    pub launcher_installed_at: Option<i64>,
    pub previous: Option<ComponentRelease>,
    pub observed: Option<ComponentObservation>,
    pub observed_at: Option<i64>,
    pub installed_at: Option<i64>,
    pub state: String,
    pub last_error: Option<String>,
}

fn validate_component(component: &str) -> Result<()> {
    anyhow::ensure!(
        matches!(component, "bot" | "api" | "webui"),
        "invalid component"
    );
    Ok(())
}

fn parse_release(raw: Option<String>) -> Result<Option<ComponentRelease>> {
    raw.map(|value| serde_json::from_str(&value).map_err(Into::into))
        .transpose()
}

fn parse_observation(raw: Option<String>) -> Result<Option<ComponentObservation>> {
    raw.map(|value| serde_json::from_str(&value).map_err(Into::into))
        .transpose()
}

pub(super) const COLS: &str = "component,channel,cadence,candidate,checked_at,check_error,desired,desired_launcher,installed,installed_launcher,launcher_sha256,launcher_installed_at,previous,observed,observed_at,installed_at,state,last_error";

pub(super) fn row_update(row: &Row) -> Result<ComponentUpdate> {
    Ok(ComponentUpdate {
        component: row.get(0)?,
        channel: row.get(1)?,
        cadence: row.get(2)?,
        candidate: parse_release(row.get(3)?)?,
        checked_at: row.get(4)?,
        check_error: row.get(5)?,
        desired: parse_release(row.get(6)?)?,
        desired_launcher: parse_release(row.get(7)?)?,
        installed: parse_release(row.get(8)?)?,
        installed_launcher: parse_release(row.get(9)?)?,
        launcher_sha256: row.get(10)?,
        launcher_installed_at: row.get(11)?,
        previous: parse_release(row.get(12)?)?,
        observed: parse_observation(row.get(13)?)?,
        observed_at: row.get(14)?,
        installed_at: row.get(15)?,
        state: row.get(16)?,
        last_error: row.get(17)?,
    })
}

/// Status text reads one autocommit SELECT. It does not start a transaction.
pub async fn component_updates_status(db: &Db) -> Result<Vec<ComponentUpdate>> {
    db.with_conn(|conn| {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLS} FROM component_updates ORDER BY component"
        ))?;
        stmt.query_map((), row_update)?.collect()
    })
    .await
}

pub async fn component_updates(db: &Db) -> Result<Vec<ComponentUpdate>> {
    db.with_conn(|conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let mut stmt = tx.prepare(&format!(
            "SELECT {COLS} FROM component_updates ORDER BY component"
        ))?;
        let result = stmt
            .query_map((), row_update)?
            .collect::<Result<Vec<_>>>()?;
        drop(stmt);
        tx.finish_read()?;
        Ok(result)
    })
    .await
}

pub async fn component_update(db: &Db, component: &str) -> Result<ComponentUpdate> {
    validate_component(component)?;
    let component = component.to_owned();
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let result = tx.query_row(
            &format!("SELECT {COLS} FROM component_updates WHERE component=?1"),
            params![component],
            row_update,
        )?;
        tx.finish_read()?;
        Ok(result)
    })
    .await
}

pub async fn record_update_check(
    db: &Db,
    component: &str,
    channel: &str,
    candidate: Option<ComponentRelease>,
    error: Option<String>,
    now: i64,
) -> Result<ComponentUpdate> {
    validate_component(component)?;
    if let Some(release) = &candidate {
        release.validate()?;
        anyhow::ensure!(
            release.component == component && release.channel == channel,
            "candidate component or channel mismatch"
        );
    }
    let (component, channel) = (component.to_owned(), channel.to_owned());
    let component_id = component.clone();
    let candidate = candidate.map(|r| serde_json::to_string(&r)).transpose()?;
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        let changed = if let Some(error) = error {
            tx.execute_rows("UPDATE component_updates SET checked_at=?2,check_error=?3 WHERE component=?1 AND channel=?4", params![component,now,error,channel], 1)?
        } else {
            tx.execute_rows("UPDATE component_updates SET candidate=?2,checked_at=?3,check_error=NULL WHERE component=?1 AND channel=?4", params![component,candidate,now,channel], 1)?
        };
        anyhow::ensure!(changed == 1, "release channel changed during check; check again");
        // The row is read in this write transaction. A later read on another
        // connection can miss the commit until that replica catches up.
        let updated = tx.query_row(
            &format!("SELECT {COLS} FROM component_updates WHERE component=?1"),
            params![component_id],
            row_update,
        )?;
        tx.commit()?;
        Ok(updated)
    }).await
}

pub fn release_check_is_stale(checked_at: Option<i64>, now: i64) -> bool {
    checked_at.is_none_or(|checked| checked > now || now.saturating_sub(checked) >= 7200)
}

pub(super) fn checked_install_candidate(
    raw: Option<String>,
    channel: &str,
    checked_at: Option<i64>,
    check_error: Option<String>,
    digest: &str,
    now: i64,
) -> Result<ComponentRelease> {
    anyhow::ensure!(
        check_error.is_none(),
        "last release check failed; check again"
    );
    anyhow::ensure!(
        !release_check_is_stale(checked_at, now),
        "candidate check is stale; check again"
    );
    let candidate: ComponentRelease = serde_json::from_str(&raw.context("no checked candidate")?)?;
    anyhow::ensure!(candidate.digest == digest, "candidate changed; check again");
    anyhow::ensure!(
        candidate.channel == channel,
        "release channel changed; check again"
    );
    Ok(candidate)
}

/// Asset URLs contain only the source SHA. A reserved SHA with no matching
/// retained release has an unknown historical digest, so it cannot be reused.
fn webui_source_reusable(
    candidate: &ComponentRelease,
    releases: &[Option<ComponentRelease>],
    reserved: bool,
) -> bool {
    let matching = releases
        .iter()
        .flatten()
        .filter(|release| release.source_sha == candidate.source_sha)
        .collect::<Vec<_>>();
    matching
        .iter()
        .all(|release| release.digest == candidate.digest)
        && (!reserved || !matching.is_empty())
}

pub async fn request_component_install(
    db: &Db,
    component: &str,
    digest: &str,
    now: i64,
) -> Result<ComponentRelease> {
    validate_component(component)?;
    let component = component.to_owned();
    let digest = digest.to_owned();
    db.with_conn(move |conn| {
        struct InstallState {
            candidate: Option<String>,
            installed: Option<String>,
            previous: Option<String>,
            desired: Option<String>,
            channel: String,
            checked_at: Option<i64>,
            check_error: Option<String>,
        }
        let tx = conn.transaction_immediate()?;
        update_control::ensure_no_service_upgrade(&tx)?;
        let state: InstallState = tx.query_row("SELECT candidate,installed,previous,desired,channel,checked_at,check_error FROM component_updates WHERE component=?1", params![component.clone()], |row| Ok(InstallState {candidate:row.get(0)?,installed:row.get(1)?,previous:row.get(2)?,desired:row.get(3)?,channel:row.get(4)?,checked_at:row.get(5)?,check_error:row.get(6)?}))?;
        let candidate = checked_install_candidate(state.candidate, &state.channel, state.checked_at, state.check_error, &digest, now)?;
        let installed = parse_release(state.installed)?;
        anyhow::ensure!(component == "webui" || installed.as_ref().is_none_or(|release| release.digest != digest), "application release is already installed");
        if component == "webui" {
            let reserved: i64 = tx.query_row("SELECT EXISTS(SELECT 1 FROM webui_managed_assets WHERE source_sha=?1)", params![candidate.source_sha.clone()], |row| Ok(row.get(0)?))?;
            anyhow::ensure!(webui_source_reusable(&candidate, &[installed.clone(),parse_release(state.previous)?,parse_release(state.desired)?],reserved != 0), "WebUI source SHA already has a different or unknown image digest; use a new source commit");
            tx.execute_rows("INSERT OR IGNORE INTO webui_managed_assets(source_sha,created_at) VALUES (?1,?2)",params![candidate.source_sha.clone(),now], 1)?;
        }
        let desired = serde_json::to_string(&candidate)?;
        update_control::clear_rejected_candidate_if_desired_changes(&tx, &component, &desired)?;
        tx.execute_rows("UPDATE component_updates SET desired=?2,state='queued',last_error=NULL WHERE component=?1", params![component, desired], 1)?;
        tx.commit()?;
        Ok(candidate)
    }).await
}

pub async fn request_launcher_install(
    db: &Db,
    component: &str,
    digest: &str,
    now: i64,
) -> Result<ComponentRelease> {
    anyhow::ensure!(
        matches!(component, "bot" | "api"),
        "only services have a launcher"
    );
    let component = component.to_owned();
    let digest = digest.to_owned();
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        update_control::ensure_no_service_upgrade(&tx)?;
        let (candidate, installed, channel, checked_at, check_error): (Option<String>, Option<String>, String, Option<i64>, Option<String>) = tx.query_row(
            "SELECT candidate,installed_launcher,channel,checked_at,check_error FROM component_updates WHERE component=?1",
            params![component.clone()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )?;
        let candidate = checked_install_candidate(candidate, &channel, checked_at, check_error, &digest, now)?;
        let installed = parse_release(installed)?;
        anyhow::ensure!(
            installed
                .as_ref()
                .is_none_or(|release| release.digest != digest),
            "launcher release is already installed"
        );
        tx.execute_rows(
            "UPDATE component_updates SET desired_launcher=?2,last_error=NULL WHERE component=?1",
            params![component, serde_json::to_string(&candidate)?], 1,
        )?;
        tx.commit()?;
        Ok(candidate)
    })
    .await
}

pub async fn record_launcher_running(
    db: &Db,
    component: &str,
    release: &ComponentRelease,
    hash: &str,
    now: i64,
) -> Result<()> {
    anyhow::ensure!(
        matches!(component, "bot" | "api"),
        "invalid launcher component"
    );
    let (component, release, hash) = (
        component.to_owned(),
        serde_json::to_string(release)?,
        hash.to_owned(),
    );
    db.with_conn(move |conn| {
        let tx=conn.transaction_immediate()?;
        let changed=tx.execute_rows("UPDATE component_updates SET installed_launcher=?2,launcher_sha256=?3,launcher_installed_at=?4 WHERE component=?1 AND (desired_launcher=?2 OR installed_launcher=?2)",params![component,release,hash,now], 1)?;
        anyhow::ensure!(changed==1,"desired launcher changed before activation");
        tx.commit()?;
        Ok(())
    }).await
}

pub async fn record_baked_launcher(db: &Db, component: &str, hash: &str, now: i64) -> Result<()> {
    anyhow::ensure!(
        matches!(component, "bot" | "api"),
        "invalid launcher component"
    );
    anyhow::ensure!(
        hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid launcher hash"
    );
    let (component, hash) = (component.to_owned(), hash.to_owned());
    db.with_conn(move |conn| {
        conn.execute_rows("UPDATE component_updates SET installed_launcher=NULL,launcher_sha256=?2,launcher_installed_at=?3 WHERE component=?1",params![component,hash,now], 1)?;
        Ok(())
    }).await
}

pub async fn record_launcher_failure(
    db: &Db,
    component: &str,
    digest: &str,
    error: &str,
) -> Result<()> {
    anyhow::ensure!(
        matches!(component, "bot" | "api"),
        "invalid launcher component"
    );
    let (component, digest, error) = (
        component.to_owned(),
        digest.to_owned(),
        error.chars().take(300).collect::<String>(),
    );
    db.with_conn(move |conn| {
        let tx=conn.transaction_immediate()?;
        let desired:Option<String>=tx.query_row("SELECT desired_launcher FROM component_updates WHERE component=?1",params![component.clone()],|row| Ok(row.get(0)?))?;
        if desired.as_deref().is_some_and(|raw| serde_json::from_str::<ComponentRelease>(raw).is_ok_and(|release| release.digest==digest)) {
            tx.execute_rows("UPDATE component_updates SET installed_launcher=CASE WHEN installed_launcher=desired_launcher THEN NULL ELSE installed_launcher END,desired_launcher=NULL,state='failed',last_error=?2 WHERE component=?1",params![component,error], 1)?;
        }
        tx.commit()?;
        Ok(())
    }).await
}

pub async fn core_schema_version(db: &Db) -> Result<i64> {
    db.with_conn(|conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let version = tx.query_row("PRAGMA user_version", (), |row| Ok(row.get(0)?))?;
        tx.finish_read()?;
        Ok(version)
    })
    .await
}

pub async fn record_component_running(
    db: &Db,
    component: &str,
    release: Option<ComponentRelease>,
    observation: ComponentObservation,
    now: i64,
) -> Result<()> {
    validate_component(component)?;
    let component = component.to_owned();
    let release = release.map(|r| serde_json::to_string(&r)).transpose()?;
    let observation = serde_json::to_string(&observation)?;
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        if let Some(release) = release {
            let changed = tx.execute_rows("UPDATE component_updates SET previous=CASE WHEN installed IS NOT ?2 THEN installed ELSE previous END,installed=?2,installed_at=CASE WHEN installed IS NOT ?2 THEN ?3 ELSE installed_at END,observed=?4,observed_at=?3,state='running',last_error=NULL WHERE component=?1 AND (desired=?2 OR installed=?2)", params![component.clone(),release.clone(),now,observation], 1)?;
            anyhow::ensure!(changed == 1, "desired release changed before activation");
            update_control::clear_rejected_candidate_after_activation(
                &tx,
                &component,
                &release,
            )?;
        } else {
            tx.execute_rows("UPDATE component_updates SET observed=?2,observed_at=?3 WHERE component=?1", params![component,observation,now], 1)?;
        }
        tx.commit()?;
        Ok(())
    }).await
}

/// Record a verified first/baked start when there is no selected managed app.
/// Managed cold starts restore their durable selection before launching and
/// use record_component_running instead, retaining install identity and time.
pub async fn record_baked_component_running(
    db: &Db,
    component: &str,
    observation: ComponentObservation,
    now: i64,
) -> Result<()> {
    anyhow::ensure!(
        matches!(component, "bot" | "api"),
        "invalid baked component"
    );
    let (component, observation) = (component.to_owned(), serde_json::to_string(&observation)?);
    db.with_conn(move |conn| {
        conn.execute_rows(
            "UPDATE component_updates SET installed=NULL,installed_at=NULL,previous=NULL,observed=?2,observed_at=?3,state=CASE WHEN desired IS NULL THEN 'running' ELSE 'queued' END,last_error=NULL WHERE component=?1",
            params![component,observation,now], 1,
        )?;
        Ok(())
    }).await
}

/// Compare the parsed release, then update the original stored text.
/// Rows saved before upgrade metadata omit a null `upgrade`, which this
/// serializer writes. An absent or different selection is already settled,
/// including a clear saved before a lost reply. Database and JSON failures
/// still surface.
pub async fn record_component_failure(
    db: &Db,
    component: &str,
    failed: &ComponentRelease,
    error: &str,
) -> Result<()> {
    validate_component(component)?;
    anyhow::ensure!(failed.component == component, "failed component mismatch");
    let component = component.to_owned();
    let failed = failed.clone();
    let error = error.chars().take(300).collect::<String>();
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        let stored: Option<String> = tx.query_row(
            "SELECT desired FROM component_updates WHERE component=?1",
            params![component.clone()],
            |row| Ok(row.get(0)?),
        )?;
        let Some(stored) = stored else {
            tx.commit()?;
            return Ok(());
        };
        let parsed: ComponentRelease = serde_json::from_str(&stored)?;
        if parsed != failed {
            tx.commit()?;
            return Ok(());
        }
        tx.execute_rows(
            "UPDATE component_updates SET desired=CASE WHEN component='webui' THEN desired ELSE NULL END,state='failed',last_error=?3 WHERE component=?1 AND desired=?2",
            params![component.clone(), stored, error],
            1,
        )?;
        update_control::clear_rejected_candidate_conn(&tx, &component)?;
        tx.commit()?;
        Ok(())
    })
    .await
}

/// A failed replacement must no longer look installed. Preserve the last
/// healthy observation and clear the request so it cannot retry on each poll.
pub async fn record_component_rollback(
    db: &Db,
    component: &str,
    failed: &ComponentRelease,
    previous: Option<ComponentRelease>,
    observation: ComponentObservation,
    error: &str,
    now: i64,
) -> Result<()> {
    validate_component(component)?;
    anyhow::ensure!(failed.component == component, "failed component mismatch");
    let component = component.to_owned();
    let failed = serde_json::to_string(failed)?;
    let previous = previous
        .map(|release| serde_json::to_string(&release))
        .transpose()?;
    let observation = serde_json::to_string(&observation)?;
    let error = error.chars().take(300).collect::<String>();
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        tx.execute_rows("UPDATE component_updates SET installed=?2,installed_at=CASE WHEN ?2 IS NULL THEN NULL ELSE ?3 END,desired=CASE WHEN desired=?6 THEN NULL ELSE desired END,observed=?4,observed_at=?3,state=CASE WHEN desired IS NOT NULL AND desired<>?6 THEN 'queued' ELSE 'failed' END,last_error=CASE WHEN desired IS NOT NULL AND desired<>?6 THEN last_error ELSE ?5 END WHERE component=?1",params![component.clone(),previous,now,observation,error,failed], 1)?;
        update_control::clear_rejected_candidate_conn(&tx, &component)?;
        tx.commit()?;
        Ok(())
    }).await
}

pub async fn record_webui_drift(db: &Db, error: &str) -> Result<()> {
    let error = error.chars().take(300).collect::<String>();
    db.with_conn(move |conn| {
        conn.execute_rows("UPDATE component_updates SET observed=NULL,observed_at=NULL,state=CASE WHEN state IN ('queued','failed') THEN state ELSE 'drift' END,last_error=CASE WHEN state='failed' THEN last_error ELSE ?1 END WHERE component='webui'", params![error], 1)?;
        Ok(())
    }).await
}

/// Restore the installed observation only after all five Storage objects
/// match its digest-pinned package. Preserve a separate failed desired request.
pub async fn record_webui_installed_verified(
    db: &Db,
    release: &ComponentRelease,
    observation: ComponentObservation,
    now: i64,
) -> Result<()> {
    let installed = serde_json::to_string(release)?;
    let observation = serde_json::to_string(&observation)?;
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        let changed = tx.execute_rows("UPDATE component_updates SET observed=?2,observed_at=?3,state=CASE WHEN desired IS NULL THEN 'running' ELSE state END,last_error=CASE WHEN desired IS NULL THEN NULL ELSE last_error END WHERE component='webui' AND installed=?1",params![installed,observation,now], 1)?;
        anyhow::ensure!(changed == 1, "installed WebUI release changed during verification");
        tx.commit()?;
        Ok(())
    }).await
}

pub async fn track_webui_assets(db: &Db, sha: &str, now: i64) -> Result<()> {
    anyhow::ensure!(
        sha.len() == 40 && sha.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid WebUI source SHA"
    );
    let sha = sha.to_owned();
    db.with_conn(move |conn| {
        conn.execute_rows(
            "INSERT INTO webui_managed_assets(source_sha,created_at) VALUES (?1,?2) ON CONFLICT(source_sha) DO UPDATE SET created_at=excluded.created_at WHERE webui_managed_assets.created_at<0",
            params![sha, now], 1,
        )?;
        Ok(())
    })
    .await
}

pub async fn managed_webui_assets(db: &Db) -> Result<Vec<String>> {
    db.with_conn(|conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let mut stmt = tx.prepare(
            "SELECT source_sha FROM webui_managed_assets WHERE created_at>=0 ORDER BY created_at",
        )?;
        let assets = stmt
            .query_map((), |row| Ok(row.get(0)?))?
            .collect::<Result<Vec<String>>>()?;
        drop(stmt);
        tx.finish_read()?;
        Ok(assets)
    })
    .await
}

pub async fn untrack_webui_assets(db: &Db, sha: &str) -> Result<()> {
    anyhow::ensure!(
        sha.len() == 40 && sha.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid WebUI source SHA"
    );
    let sha = sha.to_owned();
    db.with_conn(move |conn| {
        conn.execute_rows(
            "UPDATE webui_managed_assets SET created_at=-1 WHERE source_sha=?1",
            params![sha],
            1,
        )?;
        Ok(())
    })
    .await
}

/// An independent deployment gate must be explicitly enabled after the public
/// release-branch policy is established. Existing cadence rows are inert while off.
pub async fn set_update_policy(
    db: &Db,
    component: &str,
    channel: &str,
    cadence: &str,
    unattended_allowed: bool,
) -> Result<ComponentUpdate> {
    validate_component(component)?;
    anyhow::ensure!(
        matches!(channel, "latest" | "stable" | "nightly"),
        "invalid channel"
    );
    anyhow::ensure!(
        matches!(cadence, "off" | "daily" | "weekly" | "monthly"),
        "invalid cadence"
    );
    anyhow::ensure!(
        unattended_allowed || cadence == "off",
        "automatic installation is disabled for this deployment"
    );
    anyhow::ensure!(
        channel != "latest" || cadence == "off",
        "automatic installation requires a promoted release channel"
    );
    let (component, channel, cadence) =
        (component.to_owned(), channel.to_owned(), cadence.to_owned());
    let component_id = component.clone();
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        tx.execute_rows(
            "UPDATE component_updates SET channel=?2,cadence=?3,candidate=CASE WHEN channel=?2 THEN candidate ELSE NULL END,checked_at=CASE WHEN channel=?2 THEN checked_at ELSE NULL END,check_error=CASE WHEN channel=?2 THEN check_error ELSE NULL END WHERE component=?1",
            params![component, channel, cadence], 1,
        )?;
        let updated = tx.query_row(
            &format!("SELECT {COLS} FROM component_updates WHERE component=?1"),
            params![component_id],
            row_update,
        )?;
        tx.commit()?;
        Ok(updated)
    })
    .await
}

/// UTC windows are deliberately coarse: a missed poll can install once in the
/// current window, while the same digest never retries unattended after a
/// failure. An owner can always request it explicitly again.
pub async fn request_due_automatic_install(
    db: &Db,
    component: &str,
    expected_digest: &str,
    now: i64,
    unattended_allowed: bool,
) -> Result<Option<ComponentRelease>> {
    if !unattended_allowed {
        return Ok(None);
    }
    validate_component(component)?;
    let component = component.to_owned();
    let expected_digest = expected_digest.to_owned();
    db.with_conn(move |conn| {
        struct AutoState {
            channel:String,cadence:String,candidate:Option<String>,installed:Option<String>,previous:Option<String>,desired:Option<String>,checked_at:Option<i64>,
            check_error:Option<String>,last_digest:Option<String>,last_window:Option<String>,lease_until:i64,
        }
        let tx=conn.transaction_immediate()?;
        if update_control::ensure_no_service_upgrade(&tx).is_err() { tx.commit()?; return Ok(None); }
        let state:AutoState = tx.query_row(
            "SELECT channel,cadence,candidate,installed,previous,desired,checked_at,check_error,auto_last_digest,auto_last_window,lease_until FROM component_updates WHERE component=?1",
            params![component.clone()],|row| Ok(AutoState {
                channel:row.get(0)?,cadence:row.get(1)?,candidate:row.get(2)?,installed:row.get(3)?,previous:row.get(4)?,desired:row.get(5)?,checked_at:row.get(6)?,
                check_error:row.get(7)?,last_digest:row.get(8)?,last_window:row.get(9)?,lease_until:row.get(10)?,
            }))?;
        let Some(window)=automatic_window(&state.cadence,now) else { tx.commit()?; return Ok(None); };
        if component != "webui" && state.lease_until > now {
            tx.commit()?;
            return Ok(None);
        }
        if state.check_error.is_some() || release_check_is_stale(state.checked_at, now) || state.last_window.as_deref()==Some(window.as_str()) {
            tx.commit()?; return Ok(None);
        }
        let Some(candidate)=state.candidate else { tx.commit()?; return Ok(None); };
        let release:ComponentRelease=serde_json::from_str(&candidate)?;
        let installed=parse_release(state.installed)?;
        let desired=parse_release(state.desired)?;
        // A selected service digest remains queued across baked restarts. Only
        // a settled request may be replaced by a later automatic candidate.
        if component != "webui" && desired.as_ref().is_some_and(|selected| installed.as_ref().is_none_or(|current| current.digest != selected.digest)) {
            tx.commit()?; return Ok(None);
        }
        if release.channel!=state.channel || release.digest!=expected_digest || state.last_digest.as_deref()==Some(release.digest.as_str()) || installed.as_ref().is_some_and(|current| current.digest==release.digest) {
            tx.commit()?; return Ok(None);
        }
        if component=="webui" {
            let reserved:i64=tx.query_row("SELECT EXISTS(SELECT 1 FROM webui_managed_assets WHERE source_sha=?1)",params![release.source_sha.clone()],|row| Ok(row.get(0)?))?;
            if !webui_source_reusable(&release,&[installed,parse_release(state.previous)?,desired],reserved!=0) {
                tx.commit()?; return Ok(None);
            }
            tx.execute_rows("INSERT OR IGNORE INTO webui_managed_assets(source_sha,created_at) VALUES (?1,?2)",params![release.source_sha.clone(),now], 1)?;
        }
        update_control::clear_rejected_candidate_if_desired_changes(&tx, &component, &candidate)?;
        tx.execute_rows("UPDATE component_updates SET desired=?2,state='queued',last_error=NULL,auto_last_digest=?3,auto_last_window=?4 WHERE component=?1",params![component,candidate,release.digest.clone(),window], 1)?;
        tx.commit()?;
        Ok(Some(release))
    }).await
}

fn automatic_window(cadence: &str, now: i64) -> Option<String> {
    use chrono::{TimeZone, Utc};
    let date = Utc.timestamp_opt(now, 0).single()?.date_naive();
    match cadence {
        "daily" => Some(date.format("%Y-%m-%d").to_string()),
        "weekly" => Some(
            date.week(chrono::Weekday::Mon)
                .first_day()
                .format("%Y-%m-%d")
                .to_string(),
        ),
        "monthly" => Some(date.format("%Y-%m").to_string()),
        _ => None,
    }
}

/// Short durable lease prevents overlapping API check/apply workers. Its owner
/// token is always unique per attempt; expiry permits recovery after a crash.
pub async fn claim_update_lease(
    db: &Db,
    component: &str,
    owner: &str,
    now: i64,
    ttl: i64,
) -> Result<bool> {
    validate_component(component)?;
    anyhow::ensure!(
        (1..=3600).contains(&ttl)
            && owner.len() == 32
            && owner.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid update lease"
    );
    let (component, owner) = (component.to_owned(), owner.to_owned());
    db.with_conn(move |conn| {
        let tx=conn.transaction_immediate()?;
        let changed=tx.execute_rows("UPDATE component_updates SET lease_owner=?2,lease_until=?3 WHERE component=?1 AND lease_until<=?4",params![component,owner,now+ttl,now], 1)?;
        tx.commit()?;
        Ok(changed==1)
    }).await
}

pub async fn release_update_lease(db: &Db, component: &str, owner: &str) -> Result<()> {
    validate_component(component)?;
    let (component, owner) = (component.to_owned(), owner.to_owned());
    db.with_conn(move |conn| {
        conn.execute_rows("UPDATE component_updates SET lease_owner=NULL,lease_until=0 WHERE component=?1 AND lease_owner=?2",params![component,owner], 1)?;
        Ok(())
    }).await
}

/// Extend only a live lease still owned by this worker. A stopped worker must
/// not reclaim an expired token after another process has begun recovery.
pub async fn renew_update_lease(
    db: &Db,
    component: &str,
    owner: &str,
    now: i64,
    ttl: i64,
) -> Result<bool> {
    validate_component(component)?;
    anyhow::ensure!((1..=3600).contains(&ttl), "invalid update lease");
    let (component, owner) = (component.to_owned(), owner.to_owned());
    db.with_conn(move |conn| {
        Ok(conn.execute_rows(
            "UPDATE component_updates SET lease_until=?3 WHERE component=?1 AND lease_owner=?2 AND lease_until>?4",
            params![component,owner,now+ttl,now], 1,
        )? == 1)
    }).await
}

/// A recovered WebUI check holds the lease after any abandoned apply lease has
/// expired. Reconciliation preserves a queued request even when Storage is
/// partially switched. Preserve completed adoption, prior failure, and newer work.
pub async fn settle_interrupted_webui(db: &Db, release: &ComponentRelease) -> Result<()> {
    let desired = serde_json::to_string(release)?;
    db.with_conn(move |conn| {
        conn.execute_rows(
            "UPDATE component_updates SET state='failed',last_error='WebUI installation was interrupted; Storage may be partially switched. Check Storage and retry' WHERE component='webui' AND desired=?1 AND state='queued'",
            params![desired], 1,
        )?;
        Ok(())
    }).await
}

pub const UPDATE_BOOT_MISMATCH_META: &str = "update_boot_mismatch_v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BootMismatchExpected {
    pub release_id: String,
    pub source_sha: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BootMismatchRecord {
    pub component: String,
    pub running: String,
    pub expected: BootMismatchExpected,
    pub reason: String,
}

pub type BootMismatchMap = std::collections::BTreeMap<String, BootMismatchRecord>;

pub fn read_boot_mismatch_map_conn(conn: &impl SqlExecutor) -> Result<BootMismatchMap> {
    let raw = meta_string_conn(conn, UPDATE_BOOT_MISMATCH_META)?;
    match raw {
        None => Ok(BootMismatchMap::new()),
        Some(value) => Ok(serde_json::from_str(&value)?),
    }
}

pub async fn read_boot_mismatch_map(db: &Db) -> Result<BootMismatchMap> {
    db.with_conn(|conn| read_boot_mismatch_map_conn(conn)).await
}

pub fn write_boot_mismatch_map_conn(conn: &impl SqlExecutor, map: &BootMismatchMap) -> Result<()> {
    if map.is_empty() {
        conn.execute_rows(
            "DELETE FROM meta WHERE key=?1",
            params![UPDATE_BOOT_MISMATCH_META],
            1,
        )?;
        return Ok(());
    }
    write_meta_string_conn(
        conn,
        UPDATE_BOOT_MISMATCH_META,
        &serde_json::to_string(map)?,
    )?;
    Ok(())
}

pub async fn record_boot_mismatch(db: &Db, record: BootMismatchRecord) -> Result<()> {
    db.with_conn(move |conn| {
        let mut map = read_boot_mismatch_map_conn(conn)?;
        map.insert(record.component.clone(), record);
        write_boot_mismatch_map_conn(conn, &map)?;
        Ok(())
    })
    .await
}

pub async fn clear_boot_mismatch(db: &Db, component: &str) -> Result<()> {
    let component = component.to_owned();
    db.with_conn(move |conn| {
        let mut map = read_boot_mismatch_map_conn(conn)?;
        if map.remove(&component).is_some() {
            write_boot_mismatch_map_conn(conn, &map)?;
        }
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(sha: char) -> ComponentRelease {
        ComponentRelease {
            upgrade: None,
            component: "api".into(),
            channel: "latest".into(),
            release_id: format!("v1.0.{sha}"),
            source_sha: sha.to_string().repeat(40),
            published_at: "2026-09-23T00:00:00Z".into(),
            image: "ghcr.io/jatmn/jmodguard-api".into(),
            digest: format!("sha256:{}", sha.to_string().repeat(64)),
            min_schema: 15,
            max_schema: 15,
            result_schema: 15,
            min_api: 1,
            max_api: 1,
            launcher_protocol: 1,
            launcher: Some(crate::db::LauncherCompatibility {
                revision: crate::db::LAUNCHER_REVISION,
                min_revision: crate::db::REQUIRED_LAUNCHER_REVISION,
            }),
            base_abi: "debian-bookworm-amd64".into(),
        }
    }

    #[test]
    fn automatic_windows_use_utc_day_monday_and_first_of_month() {
        let at = |raw: &str| {
            chrono::DateTime::parse_from_rfc3339(raw)
                .unwrap()
                .timestamp()
        };
        assert_eq!(
            automatic_window("daily", at("2026-09-27T23:59:00Z")),
            Some("2026-09-27".into())
        );
        assert_eq!(
            automatic_window("weekly", at("2026-09-27T23:59:00Z")),
            Some("2026-09-21".into())
        );
        assert_eq!(
            automatic_window("weekly", at("2026-09-28T00:00:00Z")),
            Some("2026-09-28".into())
        );
        assert_eq!(
            automatic_window("monthly", at("2026-09-30T23:59:00Z")),
            Some("2026-09".into())
        );
        assert_eq!(
            automatic_window("monthly", at("2026-10-01T00:00:00Z")),
            Some("2026-10".into())
        );
    }

    #[tokio::test]
    async fn automatic_admission_uses_current_channel_digest_and_webui_source_identity() {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_auto_identity_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-23T01:00:00Z")
            .unwrap()
            .timestamp();
        let mut stable = candidate('a');
        stable.channel = "stable".into();
        set_update_policy(&db, "api", "stable", "daily", true)
            .await
            .unwrap();
        record_update_check(&db, "api", "stable", Some(stable.clone()), None, now)
            .await
            .unwrap();
        set_update_policy(&db, "api", "nightly", "daily", true)
            .await
            .unwrap();
        assert!(
            component_update(&db, "api")
                .await
                .unwrap()
                .candidate
                .is_none()
        );
        assert!(
            record_update_check(&db, "api", "stable", Some(stable.clone()), None, now)
                .await
                .is_err()
        );
        assert!(
            request_due_automatic_install(&db, "api", &stable.digest, now, true)
                .await
                .unwrap()
                .is_none()
        );

        let mut nightly = stable.clone();
        nightly.channel = "nightly".into();
        record_update_check(&db, "api", "nightly", Some(nightly.clone()), None, now)
            .await
            .unwrap();
        let installed = serde_json::to_string(&nightly).unwrap();
        db.with_conn(move |conn| {
            conn.execute_rows(
                "UPDATE component_updates SET installed=?1 WHERE component='api'",
                params![installed],
                1,
            )?;
            Ok(())
        })
        .await
        .unwrap();
        nightly.release_id = "different metadata, same image".into();
        record_update_check(&db, "api", "nightly", Some(nightly.clone()), None, now)
            .await
            .unwrap();
        assert!(
            request_due_automatic_install(&db, "api", &nightly.digest, now, true)
                .await
                .unwrap()
                .is_none()
        );

        let mut webui = candidate('b');
        webui.component = "webui".into();
        webui.image = "ghcr.io/jatmn/jmodguard-webui".into();
        webui.base_abi = "static-webui-v1".into();
        webui.channel = "stable".into();
        let mut repack = webui.clone();
        repack.digest = format!("sha256:{}", "c".repeat(64));
        set_update_policy(&db, "webui", "stable", "daily", true)
            .await
            .unwrap();
        record_update_check(&db, "webui", "stable", Some(repack.clone()), None, now)
            .await
            .unwrap();
        let installed = serde_json::to_string(&webui).unwrap();
        db.with_conn(move |conn| {
            conn.execute_rows(
                "UPDATE component_updates SET installed=?1 WHERE component='webui'",
                params![installed],
                1,
            )?;
            Ok(())
        })
        .await
        .unwrap();
        assert!(
            request_due_automatic_install(&db, "webui", &repack.digest, now, true)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            request_component_install(&db, "webui", &repack.digest, now)
                .await
                .is_err()
        );

        // A's URLs remain cacheable when B replaces it. A repack must not
        // overwrite those URLs through either admission path.
        let mut replacement = candidate('d');
        replacement.component = "webui".into();
        replacement.image = webui.image.clone();
        replacement.base_abi = webui.base_abi.clone();
        replacement.channel = "stable".into();
        let old = serde_json::to_string(&webui).unwrap();
        let current = serde_json::to_string(&replacement).unwrap();
        db.with_conn(move |conn| {
            conn.execute_rows(
                "UPDATE component_updates SET installed=?1,previous=?2 WHERE component='webui'",
                params![current, old],
                1,
            )?;
            Ok(())
        })
        .await
        .unwrap();
        assert!(
            request_due_automatic_install(&db, "webui", &repack.digest, now, true)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            request_component_install(&db, "webui", &repack.digest, now)
                .await
                .is_err()
        );

        // A matching digest can repair a retained release, but a retired SHA
        // without a known matching digest must stay reserved.
        track_webui_assets(&db, &webui.source_sha, now)
            .await
            .unwrap();
        record_update_check(&db, "webui", "stable", Some(webui.clone()), None, now)
            .await
            .unwrap();
        request_component_install(&db, "webui", &webui.digest, now)
            .await
            .unwrap();
        let orphan = candidate('e');
        track_webui_assets(&db, &orphan.source_sha, now)
            .await
            .unwrap();
        untrack_webui_assets(&db, &orphan.source_sha).await.unwrap();
        assert!(
            !managed_webui_assets(&db)
                .await
                .unwrap()
                .contains(&orphan.source_sha)
        );
        let mut orphan = orphan;
        orphan.component = "webui".into();
        orphan.image = webui.image.clone();
        orphan.base_abi = webui.base_abi.clone();
        orphan.channel = "stable".into();
        record_update_check(&db, "webui", "stable", Some(orphan.clone()), None, now)
            .await
            .unwrap();
        assert!(
            request_component_install(&db, "webui", &orphan.digest, now)
                .await
                .is_err()
        );
        assert!(
            request_due_automatic_install(&db, "webui", &orphan.digest, now, true)
                .await
                .unwrap()
                .is_none()
        );
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn lease_renewal_and_interrupted_webui_settlement_keep_owner_identity() {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_lease_recovery_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        let a = "a".repeat(32);
        let b = "b".repeat(32);
        assert!(claim_update_lease(&db, "webui", &a, 100, 10).await.unwrap());
        assert!(renew_update_lease(&db, "webui", &a, 105, 10).await.unwrap());
        assert!(!claim_update_lease(&db, "webui", &b, 110, 10).await.unwrap());
        assert!(claim_update_lease(&db, "webui", &b, 115, 10).await.unwrap());
        assert!(!renew_update_lease(&db, "webui", &a, 116, 10).await.unwrap());

        let mut release = candidate('c');
        release.component = "webui".into();
        release.image = "ghcr.io/jatmn/jmodguard-webui".into();
        release.base_abi = "static-webui-v1".into();
        record_update_check(&db, "webui", "latest", Some(release.clone()), None, 116)
            .await
            .unwrap();
        request_component_install(&db, "webui", &release.digest, 116)
            .await
            .unwrap();
        settle_interrupted_webui(&db, &release).await.unwrap();
        let state = component_update(&db, "webui").await.unwrap();
        assert_eq!(state.state, "failed");
        assert!(state.last_error.unwrap().contains("interrupted"));
        assert_eq!(state.desired.unwrap().digest, release.digest);
        let mut other = release.clone();
        other.digest = format!("sha256:{}", "d".repeat(64));
        settle_interrupted_webui(&db, &other).await.unwrap();
        assert_eq!(
            component_update(&db, "webui").await.unwrap().state,
            "failed"
        );
        // A crash after the admin HTML switch can clear the observation. The
        // request still needs the interrupted result, not an unrelated drift.
        request_component_install(&db, "webui", &release.digest, 117)
            .await
            .unwrap();
        record_webui_drift(&db, "Storage files differ")
            .await
            .unwrap();
        assert_eq!(
            component_update(&db, "webui").await.unwrap().state,
            "queued"
        );
        settle_interrupted_webui(&db, &release).await.unwrap();
        let partial = component_update(&db, "webui").await.unwrap();
        assert_eq!(partial.state, "failed");
        assert!(partial.last_error.unwrap().contains("partially switched"));
        record_webui_drift(&db, "Storage still differs")
            .await
            .unwrap();
        assert_eq!(
            component_update(&db, "webui").await.unwrap().state,
            "failed"
        );
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn restored_installed_webui_keeps_interrupted_request_failed() {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_webui_restore_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        let mut old = candidate('a');
        old.component = "webui".into();
        old.image = "ghcr.io/jatmn/jmodguard-webui".into();
        old.base_abi = "static-webui-v1".into();
        let mut next = candidate('b');
        next.component = "webui".into();
        next.image = old.image.clone();
        next.base_abi = old.base_abi.clone();
        let observation = ComponentObservation {
            guild_schema: None,
            update_protocol: 0,
            launcher_revision: 0,
            source_sha: old.source_sha.clone(),
            executable_sha256: "c".repeat(64),
            admin_sha256: Some("d".repeat(64)),
            base_image_sha: "Storage Zone".into(),
            started_at: 100,
            min_schema: 15,
            max_schema: 15,
            api_capability: 1,
            self_update_enabled: true,
            base_abi: "static-webui-v1".into(),
        };
        record_update_check(&db, "webui", "latest", Some(old.clone()), None, 100)
            .await
            .unwrap();
        request_component_install(&db, "webui", &old.digest, 100)
            .await
            .unwrap();
        record_component_running(&db, "webui", Some(old.clone()), observation.clone(), 101)
            .await
            .unwrap();
        record_update_check(&db, "webui", "latest", Some(next.clone()), None, 110)
            .await
            .unwrap();
        request_component_install(&db, "webui", &next.digest, 110)
            .await
            .unwrap();
        record_webui_drift(&db, "Storage files differ")
            .await
            .unwrap();
        settle_interrupted_webui(&db, &next).await.unwrap();
        record_webui_installed_verified(&db, &old, observation.clone(), 120)
            .await
            .unwrap();
        let state = component_update(&db, "webui").await.unwrap();
        assert_eq!(state.state, "failed");
        assert_eq!(state.observed.unwrap().source_sha, old.source_sha);
        assert_eq!(state.desired.unwrap().digest, next.digest);
        assert!(state.last_error.unwrap().contains("interrupted"));
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn state_is_independent_and_unattended_gate_fails_closed() {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_updates_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        let rows = component_updates(&db).await.unwrap();
        assert_eq!(rows.len(), 3);
        assert!(
            rows.iter()
                .all(|row| row.cadence == "off" && row.channel == "latest")
        );
        assert!(
            set_update_policy(&db, "api", "stable", "daily", false)
                .await
                .is_err()
        );
        assert!(
            set_update_policy(&db, "api", "latest", "daily", true)
                .await
                .is_err()
        );
        set_update_policy(&db, "api", "nightly", "off", false)
            .await
            .unwrap();
        assert_eq!(
            component_update(&db, "api").await.unwrap().channel,
            "nightly"
        );
        assert_eq!(
            component_update(&db, "bot").await.unwrap().channel,
            "latest"
        );
        let owner = "a".repeat(32);
        assert!(
            claim_update_lease(&db, "api", &owner, 100, 60)
                .await
                .unwrap()
        );
        assert!(
            !claim_update_lease(&db, "api", &"b".repeat(32), 101, 60)
                .await
                .unwrap()
        );
        release_update_lease(&db, "api", &owner).await.unwrap();
        assert!(
            claim_update_lease(&db, "api", &"b".repeat(32), 101, 60)
                .await
                .unwrap()
        );
        let managed = "c".repeat(40);
        track_webui_assets(&db, &managed, 101).await.unwrap();
        assert_eq!(
            managed_webui_assets(&db).await.unwrap(),
            vec![managed.clone()]
        );
        untrack_webui_assets(&db, &managed).await.unwrap();
        assert!(managed_webui_assets(&db).await.unwrap().is_empty());
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn exact_candidate_and_rollback_identity_survive_heartbeat() {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_updates_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        let first = candidate('a');
        record_update_check(&db, "api", &first.channel, Some(first.clone()), None, 100)
            .await
            .unwrap();
        assert!(
            request_component_install(&db, "api", &candidate('b').digest, 100)
                .await
                .is_err()
        );
        request_component_install(&db, "api", &first.digest, 100)
            .await
            .unwrap();
        request_launcher_install(&db, "api", &first.digest, 100)
            .await
            .unwrap();
        record_launcher_failure(&db, "api", &candidate('b').digest, "wrong request")
            .await
            .unwrap();
        assert!(
            component_update(&db, "api")
                .await
                .unwrap()
                .desired_launcher
                .is_some()
        );
        record_launcher_failure(&db, "api", &first.digest, "launcher failed")
            .await
            .unwrap();
        assert!(
            component_update(&db, "api")
                .await
                .unwrap()
                .desired_launcher
                .is_none()
        );
        let observation = ComponentObservation {
            guild_schema: None,
            update_protocol: 0,
            launcher_revision: 0,
            source_sha: first.source_sha.clone(),
            executable_sha256: "a".repeat(64),
            admin_sha256: None,
            base_image_sha: "b".repeat(40),
            started_at: 110,
            min_schema: 15,
            max_schema: 15,
            api_capability: 1,
            self_update_enabled: true,
            base_abi: "debian-bookworm-amd64".into(),
        };
        record_component_running(&db, "api", Some(first.clone()), observation.clone(), 111)
            .await
            .unwrap();
        let second = candidate('b');
        record_update_check(&db, "api", &second.channel, Some(second.clone()), None, 120)
            .await
            .unwrap();
        request_component_install(&db, "api", &second.digest, 120)
            .await
            .unwrap();
        let next = ComponentObservation {
            source_sha: second.source_sha.clone(),
            started_at: 121,
            ..observation
        };
        record_component_running(&db, "api", Some(second.clone()), next.clone(), 122)
            .await
            .unwrap();
        record_component_running(&db, "api", None, next, 123)
            .await
            .unwrap();
        let state = component_update(&db, "api").await.unwrap();
        assert_eq!(state.installed.unwrap().digest, second.digest);
        assert_eq!(state.previous.unwrap().digest, first.digest);
        assert_eq!(state.installed_at, Some(122));
        assert_eq!(state.observed_at, Some(123));
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn unattended_window_never_retries_failed_digest_and_rollback_clears_install() {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_updates_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        let mut first = candidate('a');
        first.channel = "stable".into();
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-23T01:00:00Z")
            .unwrap()
            .timestamp();
        set_update_policy(&db, "api", "stable", "daily", true)
            .await
            .unwrap();
        record_update_check(&db, "api", &first.channel, Some(first.clone()), None, now)
            .await
            .unwrap();
        assert!(
            request_due_automatic_install(&db, "api", &first.digest, now, false)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            request_due_automatic_install(&db, "api", &first.digest, now, true)
                .await
                .unwrap()
                .is_some()
        );
        let observed = ComponentObservation {
            guild_schema: None,
            update_protocol: 0,
            launcher_revision: 0,
            source_sha: first.source_sha.clone(),
            executable_sha256: "a".repeat(64),
            admin_sha256: None,
            base_image_sha: "b".repeat(40),
            started_at: now,
            min_schema: 15,
            max_schema: 15,
            api_capability: 1,
            self_update_enabled: true,
            base_abi: "debian-bookworm-amd64".into(),
        };
        record_component_running(&db, "api", Some(first.clone()), observed.clone(), now)
            .await
            .unwrap();
        record_component_rollback(
            &db,
            "api",
            &first,
            None,
            ComponentObservation {
                source_sha: "b".repeat(40),
                ..observed
            },
            "child exited",
            now + 1,
        )
        .await
        .unwrap();
        let state = component_update(&db, "api").await.unwrap();
        assert!(state.installed.is_none() && state.desired.is_none());
        assert!(state.installed_at.is_none());
        assert_eq!(state.state, "failed");
        assert!(
            request_due_automatic_install(&db, "api", &first.digest, now + 86400, true)
                .await
                .unwrap()
                .is_none()
        );
        let mut next = candidate('c');
        next.channel = "stable".into();
        record_update_check(
            &db,
            "api",
            &next.channel,
            Some(next.clone()),
            None,
            now + 86400,
        )
        .await
        .unwrap();
        assert!(
            request_due_automatic_install(&db, "api", &next.digest, now + 86400, true)
                .await
                .unwrap()
                .is_some()
        );
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn stale_failure_and_rollback_preserve_newer_accepted_request() {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_updates_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        let first = candidate('a');
        let next = candidate('b');
        record_update_check(&db, "api", &first.channel, Some(first.clone()), None, 100)
            .await
            .unwrap();
        request_component_install(&db, "api", &first.digest, 100)
            .await
            .unwrap();
        record_update_check(&db, "api", &next.channel, Some(next.clone()), None, 101)
            .await
            .unwrap();
        request_component_install(&db, "api", &next.digest, 101)
            .await
            .unwrap();
        record_component_failure(&db, "api", &first, "stale download failure")
            .await
            .unwrap();
        let state = component_update(&db, "api").await.unwrap();
        assert_eq!(state.desired.unwrap().digest, next.digest);
        assert_eq!(state.state, "queued");

        let observation = ComponentObservation {
            guild_schema: None,

            update_protocol: 0,

            launcher_revision: 0,
            source_sha: "c".repeat(40),
            executable_sha256: "d".repeat(64),
            admin_sha256: None,
            base_image_sha: "c".repeat(40),
            started_at: 102,
            min_schema: 15,
            max_schema: 15,
            api_capability: 1,
            self_update_enabled: true,
            base_abi: "debian-bookworm-amd64".into(),
        };
        record_component_rollback(&db, "api", &first, None, observation, "stale rollback", 103)
            .await
            .unwrap();
        let state = component_update(&db, "api").await.unwrap();
        assert_eq!(state.desired.unwrap().digest, next.digest);
        assert_eq!(state.state, "queued");
        assert!(state.last_error.is_none());
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn baked_restart_clears_managed_identity_but_preserves_pending_install() {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_updates_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        let first = candidate('a');
        let second = candidate('b');
        let observed = ComponentObservation {
            guild_schema: None,
            update_protocol: 0,
            launcher_revision: 0,
            source_sha: first.source_sha.clone(),
            executable_sha256: "c".repeat(64),
            admin_sha256: None,
            base_image_sha: first.source_sha.clone(),
            started_at: 100,
            min_schema: 15,
            max_schema: 15,
            api_capability: 1,
            self_update_enabled: true,
            base_abi: "debian-bookworm-amd64".into(),
        };
        record_update_check(&db, "api", &first.channel, Some(first.clone()), None, 100)
            .await
            .unwrap();
        request_component_install(&db, "api", &first.digest, 100)
            .await
            .unwrap();
        request_launcher_install(&db, "api", &first.digest, 100)
            .await
            .unwrap();
        record_component_running(&db, "api", Some(first.clone()), observed.clone(), 101)
            .await
            .unwrap();
        record_launcher_running(&db, "api", &first, &"d".repeat(64), 101)
            .await
            .unwrap();
        assert!(
            request_component_install(&db, "api", &first.digest, 101)
                .await
                .is_err()
        );
        assert!(
            request_launcher_install(&db, "api", &first.digest, 101)
                .await
                .is_err()
        );
        let current = component_update(&db, "api").await.unwrap();
        assert_eq!(current.state, "running");
        assert_eq!(current.desired.unwrap().digest, first.digest);

        record_update_check(&db, "api", &second.channel, Some(second.clone()), None, 102)
            .await
            .unwrap();
        request_component_install(&db, "api", &second.digest, 102)
            .await
            .unwrap();
        let baked = ComponentObservation {
            started_at: 200,
            executable_sha256: "e".repeat(64),
            ..observed.clone()
        };
        record_baked_component_running(&db, "api", baked.clone(), 201)
            .await
            .unwrap();
        record_baked_launcher(&db, "api", &"f".repeat(64), 201)
            .await
            .unwrap();
        let state = component_update(&db, "api").await.unwrap();
        assert!(state.installed.is_none() && state.installed_at.is_none());
        assert!(state.previous.is_none() && state.installed_launcher.is_none());
        assert_eq!(state.desired.unwrap().digest, second.digest);
        assert_eq!(state.state, "queued");
        assert_eq!(state.observed.unwrap().started_at, 200);
        assert_eq!(state.observed_at, Some(201));

        // A normal heartbeat must not clear a managed release after it runs.
        let managed_second = ComponentObservation {
            source_sha: second.source_sha.clone(),
            started_at: 202,
            ..observed
        };
        record_component_running(
            &db,
            "api",
            Some(second.clone()),
            managed_second.clone(),
            202,
        )
        .await
        .unwrap();
        record_component_running(&db, "api", None, managed_second, 203)
            .await
            .unwrap();
        let state = component_update(&db, "api").await.unwrap();
        assert_eq!(state.installed.unwrap().digest, second.digest);
        assert_eq!(state.installed_at, Some(202));
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn automatic_checks_preserve_pending_service_selection_until_it_runs() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-23T01:00:00Z")
            .unwrap()
            .timestamp();
        for component in ["bot", "api"] {
            let path = std::env::temp_dir().join(format!(
                "jmodguard_pending_{component}_{:032x}.db",
                rand::random::<u128>()
            ));
            let db = Db::open(&path, 2).unwrap();
            let mut selected = candidate('a');
            selected.component = component.into();
            selected.channel = "stable".into();
            selected.image = format!("ghcr.io/jatmn/jmodguard-{component}");
            let mut newer = candidate('b');
            newer.component = component.into();
            newer.channel = "stable".into();
            newer.image = format!("ghcr.io/jatmn/jmodguard-{component}");
            let observation = ComponentObservation {
                guild_schema: None,
                update_protocol: 0,
                launcher_revision: 0,
                source_sha: selected.source_sha.clone(),
                executable_sha256: "c".repeat(64),
                admin_sha256: None,
                base_image_sha: selected.source_sha.clone(),
                started_at: now,
                min_schema: 15,
                max_schema: 15,
                api_capability: 1,
                self_update_enabled: true,
                base_abi: "debian-bookworm-amd64".into(),
            };
            set_update_policy(&db, component, "stable", "daily", true)
                .await
                .unwrap();
            record_update_check(&db, component, "stable", Some(selected.clone()), None, now)
                .await
                .unwrap();
            request_component_install(&db, component, &selected.digest, now)
                .await
                .unwrap();
            record_baked_component_running(&db, component, observation.clone(), now + 1)
                .await
                .unwrap();
            record_update_check(&db, component, "stable", Some(newer.clone()), None, now + 2)
                .await
                .unwrap();

            assert!(
                request_due_automatic_install(&db, component, &newer.digest, now + 2, true)
                    .await
                    .unwrap()
                    .is_none(),
                "automatic {component} check displaced an outstanding owner selection"
            );
            let state = component_update(&db, component).await.unwrap();
            assert_eq!(state.desired.unwrap().digest, selected.digest);
            assert_eq!(state.state, "queued");

            record_component_running(&db, component, Some(selected), observation, now + 3)
                .await
                .unwrap();
            assert_eq!(
                request_due_automatic_install(&db, component, &newer.digest, now + 3, true)
                    .await
                    .unwrap()
                    .unwrap()
                    .digest,
                newer.digest,
                "settled {component} selection must not block later automatic releases"
            );
            drop(db);
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
            }
        }
    }

    #[test]
    fn release_check_is_stale_matches_install_gate() {
        let now = 10_000;
        assert!(release_check_is_stale(None, now));
        assert!(release_check_is_stale(Some(now + 1), now));
        assert!(!release_check_is_stale(Some(now - 7_199), now));
        assert!(release_check_is_stale(Some(now - 7_200), now));
    }

    #[tokio::test]
    async fn automatic_install_skips_while_update_lease_is_held() {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_updates_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-23T01:00:00Z")
            .unwrap()
            .timestamp();
        let mut release = candidate('a');
        release.component = "bot".into();
        release.channel = "stable".into();
        release.image = "ghcr.io/jatmn/jmodguard-bot".into();
        set_update_policy(&db, "bot", "stable", "daily", true)
            .await
            .unwrap();
        record_update_check(&db, "bot", "stable", Some(release.clone()), None, now)
            .await
            .unwrap();
        let owner = "a".repeat(32);
        assert!(
            claim_update_lease(&db, "bot", &owner, now, 240)
                .await
                .unwrap()
        );
        assert!(
            request_due_automatic_install(&db, "bot", &release.digest, now, true)
                .await
                .unwrap()
                .is_none()
        );
        let state = component_update(&db, "bot").await.unwrap();
        assert_ne!(state.state, "queued");
        release_update_lease(&db, "bot", &owner).await.unwrap();
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn owner_install_transaction_requires_a_current_successful_channel_check() {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_updates_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        let api = candidate('a');
        record_update_check(&db, "api", &api.channel, Some(api.clone()), None, 100)
            .await
            .unwrap();
        assert!(
            request_component_install(&db, "api", &api.digest, 7300)
                .await
                .is_err()
        );
        assert!(
            request_launcher_install(&db, "api", &api.digest, 7300)
                .await
                .is_err()
        );
        assert!(
            component_update(&db, "api")
                .await
                .unwrap()
                .desired
                .is_none()
        );

        record_update_check(&db, "api", &api.channel, Some(api.clone()), None, 7310)
            .await
            .unwrap();
        set_update_policy(&db, "api", "nightly", "off", false)
            .await
            .unwrap();
        assert!(
            request_component_install(&db, "api", &api.digest, 7310)
                .await
                .is_err()
        );
        assert!(
            request_launcher_install(&db, "api", &api.digest, 7310)
                .await
                .is_err()
        );
        set_update_policy(&db, "api", "latest", "off", false)
            .await
            .unwrap();
        record_update_check(
            &db,
            "api",
            &api.channel,
            None,
            Some("registry unavailable".into()),
            7311,
        )
        .await
        .unwrap();
        assert!(
            request_component_install(&db, "api", &api.digest, 7311)
                .await
                .is_err()
        );
        assert!(
            request_launcher_install(&db, "api", &api.digest, 7311)
                .await
                .is_err()
        );
        record_update_check(&db, "api", &api.channel, Some(api.clone()), None, 7312)
            .await
            .unwrap();
        request_component_install(&db, "api", &api.digest, 7312)
            .await
            .unwrap();
        request_launcher_install(&db, "api", &api.digest, 7312)
            .await
            .unwrap();

        let mut webui = candidate('b');
        webui.component = "webui".into();
        webui.image = "ghcr.io/jatmn/jmodguard-webui".into();
        webui.base_abi = "static-webui-v1".into();
        record_update_check(&db, "webui", &webui.channel, Some(webui.clone()), None, 200)
            .await
            .unwrap();
        assert!(
            request_component_install(&db, "webui", &webui.digest, 7400)
                .await
                .is_err()
        );
        record_update_check(
            &db,
            "webui",
            &webui.channel,
            Some(webui.clone()),
            None,
            7401,
        )
        .await
        .unwrap();
        request_component_install(&db, "webui", &webui.digest, 7401)
            .await
            .unwrap();
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn clearing_last_boot_mismatch_deletes_meta_key() {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_boot_mismatch_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        let record = BootMismatchRecord {
            component: "api".into(),
            running: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .into(),
            expected: BootMismatchExpected {
                release_id: "release".into(),
                source_sha: "b".repeat(40),
            },
            reason: "bad image signature".into(),
        };
        record_boot_mismatch(&db, record).await.unwrap();
        assert!(
            read_boot_mismatch_map(&db)
                .await
                .unwrap()
                .contains_key("api")
        );
        db.with_conn(|conn| {
            assert!(meta_string_conn(conn, UPDATE_BOOT_MISMATCH_META)?.is_some());
            Ok(())
        })
        .await
        .unwrap();
        clear_boot_mismatch(&db, "api").await.unwrap();
        assert!(read_boot_mismatch_map(&db).await.unwrap().is_empty());
        db.with_conn(|conn| {
            assert!(meta_string_conn(conn, UPDATE_BOOT_MISMATCH_META)?.is_none());
            Ok(())
        })
        .await
        .unwrap();
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}
