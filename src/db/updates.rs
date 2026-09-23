//! Durable release discovery and installation coordination for the three
//! independently deployed components. No Bunny account credential is stored.
use super::*;
use serde::Deserialize;

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
    pub base_abi: String,
}

impl ComponentRelease {
    pub fn validate(&self) -> Result<()> {
        validate_component(&self.component)?;
        anyhow::ensure!(
            matches!(self.channel.as_str(), "stable" | "nightly"),
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
}

#[derive(Debug, Clone, Serialize)]
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

const COLS: &str = "component,channel,cadence,candidate,checked_at,check_error,desired,desired_launcher,installed,installed_launcher,launcher_sha256,launcher_installed_at,previous,observed,observed_at,installed_at,state,last_error";

fn row_update(row: &Row) -> Result<ComponentUpdate> {
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

pub async fn component_updates(db: &Db) -> Result<Vec<ComponentUpdate>> {
    db.with_conn(|conn| {
        let tx = conn.transaction_primary_read()?;
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
        let tx = conn.transaction_primary_read()?;
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
    candidate: Option<ComponentRelease>,
    error: Option<String>,
    now: i64,
) -> Result<()> {
    validate_component(component)?;
    if let Some(release) = &candidate {
        release.validate()?;
        anyhow::ensure!(
            release.component == component,
            "candidate component mismatch"
        );
    }
    let component = component.to_owned();
    let candidate = candidate.map(|r| serde_json::to_string(&r)).transpose()?;
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        if let Some(error) = error {
            tx.execute("UPDATE component_updates SET checked_at=?2,check_error=?3 WHERE component=?1", params![component,now,error])?;
        } else {
            tx.execute("UPDATE component_updates SET candidate=?2,checked_at=?3,check_error=NULL WHERE component=?1", params![component,candidate,now])?;
        }
        tx.commit()?;
        Ok(())
    }).await
}

pub async fn request_component_install(
    db: &Db,
    component: &str,
    digest: &str,
) -> Result<ComponentRelease> {
    validate_component(component)?;
    let component = component.to_owned();
    let digest = digest.to_owned();
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        let candidate: Option<String> = tx.query_row("SELECT candidate FROM component_updates WHERE component=?1", params![component.clone()], |row| Ok(row.get(0)?))?;
        let candidate: ComponentRelease = serde_json::from_str(&candidate.context("no checked candidate")?)?;
        anyhow::ensure!(candidate.digest == digest, "candidate changed; check again");
        tx.execute("UPDATE component_updates SET desired=?2,state='queued',last_error=NULL WHERE component=?1", params![component,serde_json::to_string(&candidate)?])?;
        tx.commit()?;
        Ok(candidate)
    }).await
}

pub async fn request_launcher_install(
    db: &Db,
    component: &str,
    digest: &str,
) -> Result<ComponentRelease> {
    anyhow::ensure!(
        matches!(component, "bot" | "api"),
        "only services have a launcher"
    );
    let component = component.to_owned();
    let digest = digest.to_owned();
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        let candidate: Option<String> = tx.query_row(
            "SELECT candidate FROM component_updates WHERE component=?1",
            params![component.clone()],
            |row| Ok(row.get(0)?),
        )?;
        let candidate: ComponentRelease =
            serde_json::from_str(&candidate.context("no checked candidate")?)?;
        anyhow::ensure!(candidate.digest == digest, "candidate changed; check again");
        tx.execute(
            "UPDATE component_updates SET desired_launcher=?2,last_error=NULL WHERE component=?1",
            params![component, serde_json::to_string(&candidate)?],
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
        let changed=tx.execute("UPDATE component_updates SET installed_launcher=?2,launcher_sha256=?3,launcher_installed_at=?4 WHERE component=?1 AND desired_launcher=?2",params![component,release,hash,now])?;
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
        conn.execute("UPDATE component_updates SET installed_launcher=NULL,launcher_sha256=?2,launcher_installed_at=?3 WHERE component=?1",params![component,hash,now])?;
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
            tx.execute("UPDATE component_updates SET desired_launcher=NULL,state='failed',last_error=?2 WHERE component=?1",params![component,error])?;
        }
        tx.commit()?;
        Ok(())
    }).await
}

pub async fn core_schema_version(db: &Db) -> Result<i64> {
    db.with_conn(|conn| {
        let tx = conn.transaction_primary_read()?;
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
            let changed = tx.execute("UPDATE component_updates SET previous=CASE WHEN installed IS NOT ?2 THEN installed ELSE previous END,installed=?2,installed_at=CASE WHEN installed IS NOT ?2 THEN ?3 ELSE installed_at END,observed=?4,observed_at=?3,state='running',last_error=NULL WHERE component=?1 AND desired=?2", params![component,release,now,observation])?;
            anyhow::ensure!(changed == 1, "desired release changed before activation");
        } else {
            tx.execute("UPDATE component_updates SET observed=?2,observed_at=?3 WHERE component=?1", params![component,observation,now])?;
        }
        tx.commit()?;
        Ok(())
    }).await
}

pub async fn record_component_failure(
    db: &Db,
    component: &str,
    failed: &ComponentRelease,
    error: &str,
) -> Result<()> {
    validate_component(component)?;
    anyhow::ensure!(failed.component == component, "failed component mismatch");
    let component = component.to_owned();
    let failed = serde_json::to_string(failed)?;
    let error = error.chars().take(300).collect::<String>();
    db.with_conn(move |conn| {
        conn.execute(
            "UPDATE component_updates SET desired=CASE WHEN component='webui' THEN desired ELSE NULL END,state='failed',last_error=?3 WHERE component=?1 AND desired=?2",
            params![component, failed, error],
        )?;
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
        tx.execute("UPDATE component_updates SET installed=?2,installed_at=?3,desired=CASE WHEN desired=?6 THEN NULL ELSE desired END,observed=?4,observed_at=?3,state=CASE WHEN desired IS NOT NULL AND desired<>?6 THEN 'queued' ELSE 'failed' END,last_error=CASE WHEN desired IS NOT NULL AND desired<>?6 THEN last_error ELSE ?5 END WHERE component=?1",params![component,previous,now,observation,error,failed])?;
        tx.commit()?;
        Ok(())
    }).await
}

pub async fn record_webui_drift(db: &Db, error: &str) -> Result<()> {
    let error = error.chars().take(300).collect::<String>();
    db.with_conn(move |conn| {
        conn.execute("UPDATE component_updates SET observed=NULL,observed_at=NULL,state='drift',last_error=?1 WHERE component='webui'", params![error])?;
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
        conn.execute(
            "INSERT OR IGNORE INTO webui_managed_assets(source_sha,created_at) VALUES (?1,?2)",
            params![sha, now],
        )?;
        Ok(())
    })
    .await
}

pub async fn managed_webui_assets(db: &Db) -> Result<Vec<String>> {
    db.with_conn(|conn| {
        let tx = conn.transaction_primary_read()?;
        let mut stmt =
            tx.prepare("SELECT source_sha FROM webui_managed_assets ORDER BY created_at")?;
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
        conn.execute(
            "DELETE FROM webui_managed_assets WHERE source_sha=?1",
            params![sha],
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
) -> Result<()> {
    validate_component(component)?;
    anyhow::ensure!(matches!(channel, "stable" | "nightly"), "invalid channel");
    anyhow::ensure!(
        matches!(cadence, "off" | "daily" | "weekly" | "monthly"),
        "invalid cadence"
    );
    anyhow::ensure!(
        unattended_allowed || cadence == "off",
        "automatic installation is disabled for this deployment"
    );
    let (component, channel, cadence) =
        (component.to_owned(), channel.to_owned(), cadence.to_owned());
    db.with_conn(move |conn| {
        conn.execute(
            "UPDATE component_updates SET channel=?2,cadence=?3 WHERE component=?1",
            params![component, channel, cadence],
        )?;
        Ok(())
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
            cadence:String,candidate:Option<String>,installed:Option<String>,checked_at:Option<i64>,
            check_error:Option<String>,last_digest:Option<String>,last_window:Option<String>,
        }
        let tx=conn.transaction_immediate()?;
        let state:AutoState = tx.query_row(
            "SELECT cadence,candidate,installed,checked_at,check_error,auto_last_digest,auto_last_window FROM component_updates WHERE component=?1",
            params![component.clone()],|row| Ok(AutoState {
                cadence:row.get(0)?,candidate:row.get(1)?,installed:row.get(2)?,checked_at:row.get(3)?,
                check_error:row.get(4)?,last_digest:row.get(5)?,last_window:row.get(6)?,
            }))?;
        let Some(window)=automatic_window(&state.cadence,now) else { tx.commit()?; return Ok(None); };
        if state.check_error.is_some() || state.checked_at.is_none_or(|at| now-at>7200) || state.last_window.as_deref()==Some(window.as_str()) {
            tx.commit()?; return Ok(None);
        }
        let Some(candidate)=state.candidate else { tx.commit()?; return Ok(None); };
        let release:ComponentRelease=serde_json::from_str(&candidate)?;
        if release.digest!=expected_digest || state.last_digest.as_deref()==Some(release.digest.as_str()) || state.installed.as_deref()==Some(candidate.as_str()) {
            tx.commit()?; return Ok(None);
        }
        tx.execute("UPDATE component_updates SET desired=?2,state='queued',last_error=NULL,auto_last_digest=?3,auto_last_window=?4 WHERE component=?1",params![component,candidate,release.digest.clone(),window])?;
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
        let changed=tx.execute("UPDATE component_updates SET lease_owner=?2,lease_until=?3 WHERE component=?1 AND lease_until<=?4",params![component,owner,now+ttl,now])?;
        tx.commit()?;
        Ok(changed==1)
    }).await
}

pub async fn release_update_lease(db: &Db, component: &str, owner: &str) -> Result<()> {
    validate_component(component)?;
    let (component, owner) = (component.to_owned(), owner.to_owned());
    db.with_conn(move |conn| {
        conn.execute("UPDATE component_updates SET lease_owner=NULL,lease_until=0 WHERE component=?1 AND lease_owner=?2",params![component,owner])?;
        Ok(())
    }).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(sha: char) -> ComponentRelease {
        ComponentRelease {
            component: "api".into(),
            channel: "stable".into(),
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
                .all(|row| row.cadence == "off" && row.channel == "stable")
        );
        assert!(
            set_update_policy(&db, "api", "stable", "daily", false)
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
            "stable"
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
        record_update_check(&db, "api", Some(first.clone()), None, 100)
            .await
            .unwrap();
        assert!(
            request_component_install(&db, "api", &candidate('b').digest)
                .await
                .is_err()
        );
        request_component_install(&db, "api", &first.digest)
            .await
            .unwrap();
        request_launcher_install(&db, "api", &first.digest)
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
        record_update_check(&db, "api", Some(second.clone()), None, 120)
            .await
            .unwrap();
        request_component_install(&db, "api", &second.digest)
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
        let first = candidate('a');
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-23T01:00:00Z")
            .unwrap()
            .timestamp();
        set_update_policy(&db, "api", "stable", "daily", true)
            .await
            .unwrap();
        record_update_check(&db, "api", Some(first.clone()), None, now)
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
        assert_eq!(state.state, "failed");
        assert!(
            request_due_automatic_install(&db, "api", &first.digest, now + 86400, true)
                .await
                .unwrap()
                .is_none()
        );
        let next = candidate('c');
        record_update_check(&db, "api", Some(next.clone()), None, now + 86400)
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
        record_update_check(&db, "api", Some(first.clone()), None, 100)
            .await
            .unwrap();
        request_component_install(&db, "api", &first.digest)
            .await
            .unwrap();
        record_update_check(&db, "api", Some(next.clone()), None, 101)
            .await
            .unwrap();
        request_component_install(&db, "api", &next.digest)
            .await
            .unwrap();
        record_component_failure(&db, "api", &first, "stale download failure")
            .await
            .unwrap();
        let state = component_update(&db, "api").await.unwrap();
        assert_eq!(state.desired.unwrap().digest, next.digest);
        assert_eq!(state.state, "queued");

        let observation = ComponentObservation {
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
}
