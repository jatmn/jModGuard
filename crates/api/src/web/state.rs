//! API-owned dashboard state without moderation or compiled guild caches.
//! Discord mutations are admitted to the bot queue; this state has no bot token/client.
use super::oauth::{DiscordGuild, DiscordUser, OAuthClient};
use crate::config::ApiProcessConfig;
use crate::db::{self, Db};
use anyhow::{Context as _, Result};
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Instant;

/// Dashboard state cannot run the moderation engine.
/// ```compile_fail
/// fn moderation(state: &jmodguard_api::web::state::ApiState) {
///     let _ = &state.automod;
/// }
/// ```
pub struct ApiState {
    pub db: Db,
    pub tenant: crate::tenant::TenantRuntime,
    pub cfg: Arc<ApiProcessConfig>,
    pub oauth: Arc<OAuthClient>,
    /// Display caches only. Authorization must refresh the guild list.
    pub guild_cache: DashMap<u64, (Instant, Vec<DiscordGuild>)>,
    pub user_cache: DashMap<u64, (Instant, DiscordUser)>,
    pub preview: DashMap<String, crate::web::preview::PreviewGrant>,
    pub usage_cache: crate::web::usage::UsageCache,
    config_observation: tokio::sync::Mutex<(u64, String)>,
}

impl ApiState {
    pub fn new(cfg: ApiProcessConfig, db: Db) -> Result<Arc<Self>> {
        let oauth = OAuthClient::from_config(&cfg)?;
        match super::discord_retry::load_deadlines(&db) {
            Ok(deadlines) => oauth.restore_deadlines(deadlines),
            Err(error) => {
                tracing::warn!("discord retry deadlines were not loaded: {error:#}");
            }
        }
        oauth.attach_retry_store(db.clone());
        let oauth = Arc::new(oauth);
        let primed =
            db::get_primary_versions_sync(&db).context("failed to prime API config observation")?;
        db.require_config_version(primed.config);
        let tenant =
            crate::tenant::TenantRuntime::open(&db, &cfg.db, &cfg.guild_stores, &cfg.evidence)?;
        Ok(Arc::new(Self {
            db,
            tenant,
            cfg: Arc::new(cfg),
            oauth,
            guild_cache: DashMap::new(),
            user_cache: DashMap::new(),
            preview: DashMap::new(),
            usage_cache: crate::web::usage::UsageCache::default(),
            config_observation: tokio::sync::Mutex::new((primed.config, primed.config_revision)),
        }))
    }

    /// API requests load config from the DB directly; no compiled bot cache is
    /// needed. Preserve the read floor and primary-confirmed rollback behavior.
    pub async fn poll_config_version(&self) -> Result<bool> {
        let mut previous = self.config_observation.lock().await;
        let remote = db::get_version_state(&self.db).await?;
        if remote.config == previous.0
            && remote.config_revision == previous.1
            && remote.config >= self.db.required_config_version()
        {
            return Ok(false);
        }
        let floor = previous.0.max(self.db.required_config_version());
        let generation = self.db.config_write_generation();
        let primary = db::get_primary_versions(&self.db).await?;
        Ok(self.reconcile_primary(&mut previous, primary, floor, generation))
    }

    fn reconcile_primary(
        &self,
        previous: &mut (u64, String),
        primary: db::VersionState,
        floor: u64,
        generation: u64,
    ) -> bool {
        if !self
            .db
            .rebaseline_required_config_version(floor, primary.config, generation)
        {
            return false;
        }
        self.db.require_config_version(primary.config);
        let changed = primary.config != previous.0 || primary.config_revision != previous.1;
        *previous = (primary.config, primary.config_revision);
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        writer: Db,
        api: Arc<ApiState>,
        path: std::path::PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "jmodguard_api_state_{:032x}.db",
                rand::random::<u128>()
            ));
            let writer = Db::open(&path, 2).unwrap();
            let mut cfg = ApiProcessConfig::default();
            cfg.db.path = path.clone();
            cfg.db.pool_size = 2;
            let db = Db::open_with(cfg.db_open_options()).unwrap();
            let api = ApiState::new(cfg, db).unwrap();
            Self { writer, api, path }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
            }
            crate::tenant::remove_sidecars(&self.path);
        }
    }
    async fn prefix(db: &Db, value: &str) {
        db::patch_settings(db, 7, Some(Some(value.into())), None, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn api_observes_first_cross_process_write_without_bot_cache() {
        let f = Fixture::new();
        let baseline = f.api.db.required_config_version();
        assert!(!f.api.poll_config_version().await.unwrap());
        prefix(&f.writer, "updated").await;
        assert!(f.api.poll_config_version().await.unwrap());
        assert_eq!(f.api.db.required_config_version(), baseline + 1);
        assert_eq!(
            db::get_settings(&f.api.db, 7).await.unwrap().prefix,
            "updated"
        );
        assert!(!f.api.poll_config_version().await.unwrap());
    }

    #[tokio::test]
    async fn api_confirms_counter_reuse_and_rollback_against_primary() {
        let f = Fixture::new();
        let baseline = f.api.db.required_config_version();
        prefix(&f.writer, "different-lineage").await;
        f.writer
            .with_conn(move |conn| {
                let tx = conn.transaction_immediate()?;
                tx.execute_batch_bounded(
                    &format!("UPDATE meta SET value='{baseline}' WHERE key='config_version'"),
                    64 * 1024 * 1024,
                )?;
                tx.commit()
            })
            .await
            .unwrap();
        assert!(
            f.api.poll_config_version().await.unwrap(),
            "same counter, different revision"
        );
        assert_eq!(f.api.db.required_config_version(), baseline);
        prefix(&f.writer, "advance").await;
        assert!(f.api.poll_config_version().await.unwrap());
        assert!(f.api.db.required_config_version() > baseline);
        f.writer.with_conn(move |conn| {
            let tx = conn.transaction_immediate()?;
            tx.execute_batch_bounded(&format!("UPDATE meta SET value='{baseline}' WHERE key='config_version'; UPDATE meta SET value='restored-revision' WHERE key='config_revision'"), 64 * 1024 * 1024)?;
            tx.commit()
        }).await.unwrap();
        assert!(f.api.poll_config_version().await.unwrap());
        assert_eq!(f.api.db.required_config_version(), baseline);
        assert_eq!(f.api.config_observation.lock().await.1, "restored-revision");
    }

    #[tokio::test]
    async fn api_rejects_snapshot_that_precedes_a_concurrent_local_write() {
        let f = Fixture::new();
        let primary = db::get_primary_versions(&f.api.db).await.unwrap();
        let floor = f.api.db.required_config_version();
        let generation = f.api.db.config_write_generation();
        prefix(&f.api.db, "committed-during-read").await;
        let mut previous = f.api.config_observation.lock().await;
        let before = previous.clone();
        assert!(
            !f.api
                .reconcile_primary(&mut previous, primary, floor, generation)
        );
        assert_eq!(*previous, before);
        assert!(f.api.db.required_config_version() > floor);
        drop(previous);
        assert!(f.api.poll_config_version().await.unwrap());
        assert_eq!(
            db::get_settings(&f.api.db, 7).await.unwrap().prefix,
            "committed-during-read"
        );
    }
}
