//! Shared application state: DB, config, automod engine, OAuth client,
//! and the per-guild config cache (with global fallback + invalidation).

use anyhow::{Context as _, Result};
use dashmap::DashMap;
use parking_lot::Mutex;
use regex::Regex;
use serenity::model::id::ChannelId;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::automod::AutomodEngine;
use crate::config::Config;
use crate::db::{self, Db};
use crate::types::{CommandDef, GuildSettings, RuleDef};

/// In-flight Discord slash re-sync for one durable `slash_registry_version`.
///
/// Success is per-guild; remembering only the failures avoids re-POSTing
/// `set_commands` for every cached guild on every poll (rate-limit feedback)
/// while still refusing to commit the observed version until nothing remains.
struct SlashSyncProgress {
    target: u64,
    /// `None` = first pass (sync every cached guild). `Some` = retry set.
    remaining: Option<HashSet<u64>>,
    /// Config cache already invalidated once for this target.
    invalidated: bool,
}

/// Plan for one poll tick of slash re-sync work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashResyncPlan {
    /// Serenity guild cache is still empty (pre-READY); do not invalidate.
    Defer,
    /// Attempt `set_commands` for these guild ids. `invalidate` is true on the
    /// first non-empty attempt for this target (skip if `config_version` already
    /// cleared the cache this tick).
    Sync { guilds: Vec<u64>, invalidate: bool },
}

pub struct AppState {
    pub db: Db,
    pub cfg: Arc<Config>,
    pub configs: ConfigCache,
    pub automod: AutomodEngine,
    pub cooldowns: DashMap<(u64, u64, String), Instant>,
    #[allow(dead_code)]
    pub http: reqwest::Client,
    /// Lazy Discord REST client (bot token). Created on first API use
    /// (nickname / unban) so the bot process does not allocate unused Http.
    discord_http_cell: std::sync::OnceLock<Arc<serenity::http::Http>>,
    /// Cached bot user id after first Discord `/users/@me` (API nickname apply).
    /// `tokio::sync::OnceCell` so concurrent first callers share one REST fetch.
    bot_user_id: tokio::sync::OnceCell<serenity::model::id::UserId>,
    pub oauth: OAuthClient,
    pub start: Instant,
    pub slash_synced: AtomicBool,
    /// Last observed durable `config_version` from the DB.
    /// Primed at construction from the live DB so the first poll has no
    /// "unobserved" window where a warm cache can miss a cross-process bump.
    pub last_config_version: AtomicU64,
    /// Last observed durable `slash_registry_version` (commands table changes).
    /// Bot re-syncs Discord slash commands only when this advances — not on
    /// every generic `config_version` bump (rules/templates/settings/…).
    pub last_slash_registry_version: AtomicU64,
    force_slash_registry_retry: AtomicBool,
    /// Per-target retry state for slash re-sync (see [`SlashSyncProgress`]).
    slash_sync_progress: Mutex<Option<SlashSyncProgress>>,
    /// Per-user OAuth guild-list cache (discord_user_id -> (fetched_at, guilds)).
    pub guild_cache: DashMap<u64, (Instant, Vec<DiscordGuild>)>,
    pub user_cache: DashMap<u64, (Instant, DiscordUser)>,
}

impl AppState {
    pub fn new(cfg: Config, db: Db) -> Result<Arc<Self>> {
        let http = reqwest::Client::builder()
            .user_agent("jModGuard (discord moderation bot)")
            .timeout(Duration::from_secs(15))
            .build()
            .context("failed to build http client")?;
        let oauth = OAuthClient {
            client_id: cfg.web.oauth_client_id.clone(),
            client_secret: cfg.web.oauth_client_secret.clone(),
            redirect_uri: format!("{}/oauth/callback", cfg.web.base_url.trim_end_matches('/')),
            http: http.clone(),
        };
        // Prime from DB at construction — never leave an "unobserved" sentinel.
        // Cache population is independent of the poll task; skipping the first
        // poll invalidate (old u64::MAX heuristic) let a warm cache miss writes
        // that landed before the first successful poll.
        let (primed, primed_slash) = db::get_primary_versions_sync(&db)
            .context("failed to read primary version baselines while building AppState")?;
        Ok(Arc::new(Self {
            db,
            cfg: Arc::new(cfg),
            configs: ConfigCache::new(primed),
            automod: AutomodEngine::new(),
            cooldowns: DashMap::new(),
            oauth,
            http,
            discord_http_cell: std::sync::OnceLock::new(),
            bot_user_id: tokio::sync::OnceCell::new(),
            start: Instant::now(),
            slash_synced: AtomicBool::new(false),
            last_config_version: AtomicU64::new(primed),
            last_slash_registry_version: AtomicU64::new(primed_slash),
            force_slash_registry_retry: AtomicBool::new(false),
            slash_sync_progress: Mutex::new(None),
            guild_cache: DashMap::new(),
            user_cache: DashMap::new(),
        }))
    }

    /// Discord REST client for API-side bot-token calls (lazy).
    pub fn discord_http(&self) -> &serenity::http::Http {
        self.discord_http_cell
            .get_or_init(|| {
                let token = format!("Bot {}", self.cfg.discord.token);
                Arc::new(serenity::http::Http::new(&token))
            })
            .as_ref()
    }

    /// Bot's Discord user id (cached after first REST lookup).
    pub async fn bot_user_id(&self) -> Result<serenity::model::id::UserId> {
        self.bot_user_id
            .get_or_try_init(|| async {
                let me = self
                    .discord_http()
                    .get_current_user()
                    .await
                    .context("failed to fetch bot user id from Discord")?;
                Ok::<_, anyhow::Error>(me.id)
            })
            .await
            .copied()
    }

    /// Invalidates the config cache (call after any config write).
    pub fn invalidate_configs(&self) {
        self.configs.invalidate();
    }

    /// Loads durable `config_version` from the DB. When it differs from the
    /// last observation (primed at startup), invalidates the guild config cache.
    /// Returns `true` when a change triggered invalidation.
    pub async fn poll_config_version(&self) -> Result<bool> {
        let remote = db::get_config_version(&self.db).await?;
        let prev = self.last_config_version.load(Ordering::SeqCst);
        if remote > prev {
            self.last_config_version.store(remote, Ordering::SeqCst);
            self.configs.require_version(remote);
            return Ok(true);
        }
        if remote < prev {
            let (primary_config, primary_slash) = db::get_primary_versions(&self.db).await?;
            return Ok(self
                .reconcile_primary_versions(primary_config, primary_slash)
                .0);
        }
        Ok(false)
    }

    /// Loads durable `slash_registry_version`. Returns the remote version when
    /// it differs from the last **committed** observation (does not advance).
    ///
    /// Callers must [`commit_slash_registry_version`] only after Discord slash
    /// re-sync succeeds. Advancing before sync left failed guilds stale until
    /// the next command-registry write.
    pub async fn pending_slash_registry_version(&self) -> Result<Option<u64>> {
        let remote = db::get_slash_registry_version(&self.db).await?;
        let prev = self.last_slash_registry_version.load(Ordering::SeqCst);
        if self.force_slash_registry_retry.load(Ordering::SeqCst) || remote > prev {
            Ok(Some(remote.max(prev)))
        } else if remote < prev {
            let (primary_config, primary_slash) = db::get_primary_versions(&self.db).await?;
            let (_, slash_changed) = self.reconcile_primary_versions(primary_config, primary_slash);
            let current = self.last_slash_registry_version.load(Ordering::SeqCst);
            Ok(
                (slash_changed || self.force_slash_registry_retry.load(Ordering::SeqCst))
                    .then_some(current),
            )
        } else {
            Ok(None)
        }
    }

    fn reconcile_primary_versions(&self, config: u64, slash: u64) -> (bool, bool) {
        let previous_config = self.last_config_version.load(Ordering::SeqCst);
        let config_changed = config != previous_config
            && self
                .last_config_version
                .compare_exchange(previous_config, config, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok();
        if config_changed {
            if config < previous_config {
                tracing::warn!(
                    previous_config,
                    primary_config = config,
                    "primary database confirmed config version rollback; rebaselining after provider failover"
                );
                self.db
                    .rebaseline_required_config_version(previous_config, config);
                self.configs.rebaseline_version(previous_config, config);
            } else {
                self.configs.require_version(config);
            }
        }

        let previous_slash = self.last_slash_registry_version.load(Ordering::SeqCst);
        let slash_changed = slash != previous_slash
            && self
                .last_slash_registry_version
                .compare_exchange(previous_slash, slash, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok();
        if slash_changed {
            if slash < previous_slash {
                tracing::warn!(
                    previous_slash,
                    primary_slash = slash,
                    "primary database confirmed slash-registry rollback; forcing Discord reconciliation"
                );
            }
            self.force_slash_registry_retry
                .store(true, Ordering::SeqCst);
            *self.slash_sync_progress.lock() = None;
        }
        (config_changed, slash_changed)
    }

    /// Marks `version` as observed after a successful slash re-sync pass.
    pub fn commit_slash_registry_version(&self, version: u64) {
        self.last_slash_registry_version
            .store(version, Ordering::SeqCst);
        self.force_slash_registry_retry
            .store(false, Ordering::SeqCst);
        // Drop any stale retry set for an older target.
        let mut progress = self.slash_sync_progress.lock();
        if progress.as_ref().is_some_and(|p| p.target == version) {
            *progress = None;
        }
    }

    /// Make the next poll re-sync the current durable command registry. Used
    /// after a transient startup/guild-join sync failure, when no write may
    /// otherwise advance the durable version to trigger another attempt.
    pub fn retry_slash_registry_version(&self) {
        self.force_slash_registry_retry
            .store(true, Ordering::SeqCst);
        *self.slash_sync_progress.lock() = None;
    }

    pub fn acknowledged_slash_registry_version(&self) -> u64 {
        self.last_slash_registry_version.load(Ordering::SeqCst)
    }

    /// Decide which guilds to `set_commands` for pending registry `remote`.
    ///
    /// `cached` is the current serenity guild-id list (owned `u64`s — never hold
    /// a CacheRef across await). Empty cache → [`SlashResyncPlan::Defer`] without
    /// invalidating. A new `remote` resets progress so every guild is attempted
    /// once; later polls only retry the failure set.
    pub fn plan_slash_resync(&self, remote: u64, cached: &[u64]) -> SlashResyncPlan {
        let mut guard = self.slash_sync_progress.lock();
        if guard.as_ref().map(|p| p.target) != Some(remote) {
            *guard = Some(SlashSyncProgress {
                target: remote,
                remaining: None,
                invalidated: false,
            });
        }
        if cached.is_empty() {
            return SlashResyncPlan::Defer;
        }
        let progress = guard.as_mut().expect("progress set above");
        let guilds = match &progress.remaining {
            None => cached.to_vec(),
            Some(rem) => {
                let pending: Vec<u64> =
                    cached.iter().copied().filter(|g| rem.contains(g)).collect();
                if pending.is_empty() {
                    // A guild can disappear from Serenity's cache while it is
                    // unavailable or reconnecting. Keep its failed sync owned
                    // by this durable version until it reappears and succeeds.
                    return SlashResyncPlan::Defer;
                }
                pending
            }
        };
        let invalidate = !progress.invalidated;
        if invalidate {
            progress.invalidated = true;
        }
        SlashResyncPlan::Sync { guilds, invalidate }
    }

    /// Record guilds that failed `set_commands` for `remote`.
    /// Returns `true` when the caller should [`commit_slash_registry_version`].
    pub fn finish_slash_resync(&self, remote: u64, failed: HashSet<u64>) -> bool {
        let mut guard = self.slash_sync_progress.lock();
        let Some(progress) = guard.as_mut() else {
            return failed.is_empty();
        };
        if progress.target != remote {
            return false;
        }
        if failed.is_empty() {
            *guard = None;
            true
        } else {
            progress.remaining = Some(failed);
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Discord OAuth2 (manual, dependency-free of the oauth2 crate)
// ---------------------------------------------------------------------------

pub struct OAuthClient {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    pub http: reqwest::Client,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: u64,
    pub scope: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DiscordUser {
    pub id: String,
    pub username: String,
    pub global_name: Option<String>,
    pub avatar: Option<String>,
}

impl DiscordUser {
    pub fn avatar_url(&self) -> String {
        match &self.avatar {
            Some(a) if a.starts_with("a_") => format!(
                "https://cdn.discordapp.com/avatars/{}/{}.gif?size=128",
                self.id, a
            ),
            Some(a) => format!(
                "https://cdn.discordapp.com/avatars/{}/{}.png?size=128",
                self.id, a
            ),
            None => {
                let default = (self.id.parse::<u64>().unwrap_or(0) >> 22) % 6;
                format!("https://cdn.discordapp.com/embed/avatars/{default}.png")
            }
        }
    }
}

/// Discord returns `permissions` as an integer in `/users/@me/guilds`
/// (despite documenting it as a string), so accept both.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum PermissionsField {
    Number(u64),
    String(String),
}

impl PermissionsField {
    pub fn as_u64(&self) -> u64 {
        match self {
            PermissionsField::Number(n) => *n,
            PermissionsField::String(s) => s.parse().unwrap_or(0),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DiscordGuild {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
    pub owner: bool,
    pub permissions: Option<PermissionsField>,
}

impl DiscordGuild {
    pub fn manage_permission(&self) -> bool {
        const MANAGE_GUILD: u64 = 1 << 5;
        if self.owner {
            return true;
        }
        self.permissions
            .as_ref()
            .map(|p| p.as_u64() & MANAGE_GUILD != 0)
            .unwrap_or(false)
    }

    /// WebUI configuration rights do not imply authority to remove bans.
    pub fn ban_members_permission(&self) -> bool {
        const ADMINISTRATOR: u64 = 1 << 3;
        const BAN_MEMBERS: u64 = 1 << 2;
        self.owner
            || self
                .permissions
                .as_ref()
                .map(|p| p.as_u64() & (ADMINISTRATOR | BAN_MEMBERS) != 0)
                .unwrap_or(false)
    }
}

impl OAuthClient {
    pub fn authorize_url(&self, state: &str) -> String {
        let redirect = percent_encoding::utf8_percent_encode(
            &self.redirect_uri,
            percent_encoding::NON_ALPHANUMERIC,
        );
        format!(
            "https://discord.com/oauth2/authorize?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}",
            self.client_id, redirect, "identify%20guilds", state
        )
    }

    pub async fn exchange_code(&self, code: &str) -> Result<TokenResponse> {
        self.token_request("authorization_code", code, None).await
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<TokenResponse> {
        self.token_request("refresh_token", refresh_token, None)
            .await
    }

    async fn token_request(
        &self,
        grant_type: &str,
        value: &str,
        _code_verifier: Option<&str>,
    ) -> Result<TokenResponse> {
        let resp = self
            .http
            .post("https://discord.com/api/oauth2/token")
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("grant_type", grant_type),
                ("redirect_uri", self.redirect_uri.as_str()),
                (
                    if grant_type == "authorization_code" {
                        "code"
                    } else {
                        "refresh_token"
                    },
                    value,
                ),
            ])
            .send()
            .await
            .context("oauth token request failed")?;
        let status = resp.status();
        let bytes = resp.bytes().await?;
        if !status.is_success() {
            anyhow::bail!(
                "discord oauth error {status}: {}",
                String::from_utf8_lossy(&bytes)
            );
        }
        serde_json::from_slice(&bytes).context("invalid oauth token response")
    }

    pub async fn me(&self, access_token: &str) -> Result<DiscordUser> {
        let resp = self
            .http
            .get("https://discord.com/api/users/@me")
            .bearer_auth(access_token)
            .send()
            .await
            .context("discord me request failed")?;
        if !resp.status().is_success() {
            anyhow::bail!("discord me request failed with {}", resp.status());
        }
        resp.json().await.context("invalid discord user response")
    }

    pub async fn my_guilds(&self, access_token: &str) -> Result<Vec<DiscordGuild>> {
        // Discord paginates at 200 guilds; a single GET silently drops every
        // server past the first page and deny WebUI manage access for them.
        const PAGE_LIMIT: usize = 200;
        let mut all = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let mut url = format!("https://discord.com/api/users/@me/guilds?limit={PAGE_LIMIT}");
            if let Some(ref cursor) = after {
                url.push_str("&after=");
                url.push_str(cursor);
            }
            let resp = self
                .http
                .get(&url)
                .bearer_auth(access_token)
                .send()
                .await
                .context("discord guilds request failed")?;
            if !resp.status().is_success() {
                anyhow::bail!("discord guilds request failed with {}", resp.status());
            }
            let page: Vec<DiscordGuild> = resp
                .json()
                .await
                .context("invalid discord guilds response")?;
            match next_guilds_after_cursor(&page, PAGE_LIMIT) {
                Some(next) => {
                    all.extend(page);
                    after = Some(next);
                }
                None => {
                    all.extend(page);
                    break;
                }
            }
        }
        Ok(all)
    }
}

/// Cursor for the next Discord `/users/@me/guilds` page, or `None` when the
/// current page is the last (`len < page_limit`, including empty).
///
/// Extracted so pagination termination is unit-tested without HTTP.
pub(crate) fn next_guilds_after_cursor(page: &[DiscordGuild], page_limit: usize) -> Option<String> {
    if page.len() < page_limit {
        None
    } else {
        page.last().map(|g| g.id.clone())
    }
}

// ---------------------------------------------------------------------------
// per-guild config cache
// ---------------------------------------------------------------------------

pub struct CachedGuildConfig {
    pub version: u64,
    pub durable_version: u64,
    pub loaded_at: Instant,
    pub settings: GuildSettings,
    pub rules: HashMap<String, RuleDef>,
    pub commands: HashMap<String, CommandDef>,
    pub templates: HashMap<String, String>,
    pub log_channels: HashMap<String, ChannelId>,
    pub regexes: Vec<Regex>,
}

impl CachedGuildConfig {
    fn from_parts(version: u64, durable_version: u64, parts: db::ConfigParts) -> Self {
        let mut regexes = Vec::new();
        for pattern in &parts.badwords {
            // Same compile limits as WebUI / add_badword — legacy DB rows that
            // exceed the budget are skipped so cache load cannot DoS the bot.
            match crate::normalize::compile_user_regex(pattern) {
                Ok(re) => regexes.push(re),
                Err(e) => {
                    tracing::warn!("badword pattern `{pattern}` failed to compile: {e}");
                }
            }
        }
        Self {
            version,
            durable_version,
            loaded_at: Instant::now(),
            settings: parts.settings,
            rules: parts.rules,
            commands: parts.commands,
            templates: parts.templates,
            log_channels: parts
                .log_channels
                .into_iter()
                .filter_map(|(event, channel_id)| {
                    match u64::try_from(channel_id).ok().filter(|id| *id != 0) {
                        Some(channel_id) => Some((event, ChannelId::new(channel_id))),
                        None => {
                            tracing::warn!(
                                "ignoring invalid persisted log channel id {channel_id} for event {event}"
                            );
                            None
                        }
                    }
                })
                .collect(),
            regexes,
        }
    }

    /// Looks up a command by name or alias (case-insensitive).
    pub fn find_command(&self, name: &str) -> Option<&CommandDef> {
        let lower = name.to_lowercase();
        self.commands.values().find(|c| c.matches(&lower))
    }
}

#[derive(Clone)]
pub struct ConfigCache {
    map: DashMap<i64, Arc<CachedGuildConfig>>,
    version: Arc<AtomicU64>,
    ttl: Duration,
    durable_floor: Arc<AtomicU64>,
}

impl Default for ConfigCache {
    fn default() -> Self {
        Self::new(0)
    }
}

impl ConfigCache {
    pub fn new(durable_floor: u64) -> Self {
        Self {
            map: DashMap::new(),
            version: Arc::new(AtomicU64::new(0)),
            ttl: Duration::from_secs(15),
            durable_floor: Arc::new(AtomicU64::new(durable_floor)),
        }
    }

    pub fn invalidate(&self) {
        self.version.fetch_add(1, Ordering::Relaxed);
        self.map.clear();
    }

    pub fn require_version(&self, version: u64) {
        self.durable_floor.fetch_max(version, Ordering::SeqCst);
        self.invalidate();
    }

    fn rebaseline_version(&self, expected: u64, version: u64) {
        let mut current = self.durable_floor.load(Ordering::SeqCst);
        while current > version && current <= expected {
            match self.durable_floor.compare_exchange(
                current,
                version,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        self.invalidate();
    }

    pub async fn get(&self, db: &Db, guild_id: u64) -> Result<Arc<CachedGuildConfig>> {
        let required = self
            .durable_floor
            .load(Ordering::SeqCst)
            .max(db.required_config_version());
        if let Some(entry) = self.map.get(&(guild_id as i64))
            && entry.version == self.version.load(Ordering::Relaxed)
            && entry.loaded_at.elapsed() < self.ttl
            && entry.durable_version >= required
        {
            return Ok(Arc::clone(&entry));
        }
        let version = self.version.load(Ordering::Relaxed);
        let (durable_version, parts) =
            db::load_config_parts_with_version(db, guild_id as i64).await?;
        let required_after = self
            .durable_floor
            .load(Ordering::SeqCst)
            .max(db.required_config_version());
        let version_after = self.version.load(Ordering::Relaxed);
        if durable_version < required_after || version_after != version {
            anyhow::bail!(
                "database config snapshot changed while loading (version {durable_version}, expected at least {required_after})"
            );
        }
        let cfg = Arc::new(CachedGuildConfig::from_parts(
            version,
            durable_version,
            parts,
        ));
        self.map.insert(guild_id as i64, Arc::clone(&cfg));
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::{self, Db, DbOpenOptions};
    use crate::types::GuildSettings;

    fn tokio_test_block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn poll_config_version_invalidates_after_cross_process_write() {
        let tmp = std::env::temp_dir().join(format!(
            "jmodguard_poll_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bot_db = Db::open_with(DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: true,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .unwrap();
        let api_db = Db::open_with(DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: false,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .unwrap();
        let bot = AppState::new(Config::default(), bot_db).unwrap();
        // Already primed at construction — unchanged version must not invalidate.
        assert!(!tokio_test_block_on(bot.poll_config_version()).unwrap());
        tokio_test_block_on(db::upsert_settings(&api_db, 7, &GuildSettings::default())).unwrap();
        assert!(tokio_test_block_on(bot.poll_config_version()).unwrap());
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(format!("{}-wal", tmp.display()));
        let _ = std::fs::remove_file(format!("{}-shm", tmp.display()));
    }

    #[test]
    fn poll_invalidates_warm_cache_even_before_first_poll() {
        // Regression: old u64::MAX "first observation" skip left a warm cache
        // stale when a cross-process write landed before the first successful poll.
        let tmp = std::env::temp_dir().join(format!(
            "jmodguard_poll_warm_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bot_db = Db::open_with(DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: true,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .unwrap();
        let api_db = Db::open_with(DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: false,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .unwrap();
        let bot = AppState::new(Config::default(), bot_db).unwrap();
        let guild_id = 99u64;

        // Warm cache with default prefix — no poll yet.
        let before = tokio_test_block_on(bot.configs.get(&bot.db, guild_id)).unwrap();
        assert_eq!(before.settings.prefix, "!");

        let mut settings = GuildSettings::default();
        settings.prefix = "!api-wrote".into();
        tokio_test_block_on(db::upsert_settings(&api_db, guild_id as i64, &settings)).unwrap();

        // First poll after a write must invalidate (primed baseline ≠ remote).
        assert!(tokio_test_block_on(bot.poll_config_version()).unwrap());
        let after = tokio_test_block_on(bot.configs.get(&bot.db, guild_id)).unwrap();
        assert_eq!(after.settings.prefix, "!api-wrote");

        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(format!("{}-wal", tmp.display()));
        let _ = std::fs::remove_file(format!("{}-shm", tmp.display()));
    }

    #[test]
    fn pending_slash_registry_rebaselines_only_after_primary_confirmation() {
        let tmp = std::env::temp_dir().join(format!(
            "jmodguard_slash_pending_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let db = Db::open_with(DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: true,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .unwrap();
        let app = AppState::new(Config::default(), db).unwrap();
        let initial_slash = app.acknowledged_slash_registry_version();
        let initial_config = app.last_config_version.load(Ordering::SeqCst);
        assert_eq!(
            tokio_test_block_on(app.pending_slash_registry_version()).unwrap(),
            None
        );
        let cmd = crate::defaults::default_commands()
            .into_iter()
            .next()
            .expect("defaults");
        tokio_test_block_on(db::upsert_command(&app.db, 1, &cmd.name, &cmd)).unwrap();
        let pending = tokio_test_block_on(app.pending_slash_registry_version())
            .unwrap()
            .expect("command write must surface pending slash version");
        assert_eq!(pending, initial_slash + 1);
        // Peek again — must still be pending (no auto-advance).
        assert_eq!(
            tokio_test_block_on(app.pending_slash_registry_version()).unwrap(),
            Some(initial_slash + 1)
        );
        app.commit_slash_registry_version(pending);
        assert_eq!(
            tokio_test_block_on(app.pending_slash_registry_version()).unwrap(),
            None
        );
        app.commit_slash_registry_version(10);
        assert_eq!(
            tokio_test_block_on(app.pending_slash_registry_version()).unwrap(),
            Some(initial_slash + 1),
            "an immediate primary transaction confirms an authoritative rollback"
        );
        app.last_config_version.store(10, Ordering::SeqCst);
        app.configs.require_version(10);
        assert!(tokio_test_block_on(app.poll_config_version()).unwrap());
        assert_eq!(
            app.last_config_version.load(Ordering::SeqCst),
            initial_config + 1
        );
        tokio_test_block_on(app.configs.get(&app.db, 1)).expect("cache floor must rebaseline");
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(format!("{}-wal", tmp.display()));
        let _ = std::fs::remove_file(format!("{}-shm", tmp.display()));
    }

    #[test]
    fn slash_resync_retries_only_failures_and_defers_empty_cache() {
        let tmp = std::env::temp_dir().join(format!(
            "jmodguard_slash_plan_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let db = Db::open_with(DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: true,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .unwrap();
        let app = AppState::new(Config::default(), db).unwrap();
        let remote = 7u64;

        assert_eq!(
            app.plan_slash_resync(remote, &[]),
            SlashResyncPlan::Defer,
            "empty cache must not invalidate or sync"
        );

        match app.plan_slash_resync(remote, &[10, 20, 30]) {
            SlashResyncPlan::Sync { guilds, invalidate } => {
                assert_eq!(guilds, vec![10, 20, 30]);
                assert!(invalidate, "first non-empty pass invalidates once");
            }
            other => panic!("expected Sync, got {other:?}"),
        }
        // Second plan before finish — still full set, but no second invalidate.
        match app.plan_slash_resync(remote, &[10, 20, 30]) {
            SlashResyncPlan::Sync { guilds, invalidate } => {
                assert_eq!(guilds, vec![10, 20, 30]);
                assert!(!invalidate);
            }
            other => panic!("expected Sync, got {other:?}"),
        }

        assert!(!app.finish_slash_resync(remote, [20u64].into_iter().collect()));
        match app.plan_slash_resync(remote, &[10, 20, 30]) {
            SlashResyncPlan::Sync { guilds, invalidate } => {
                assert_eq!(guilds, vec![20], "only failures are retried");
                assert!(!invalidate);
            }
            other => panic!("expected Sync, got {other:?}"),
        }

        // A temporarily unavailable guild must keep the durable version
        // pending; otherwise its slash registry is never retried on return.
        assert_eq!(
            app.plan_slash_resync(remote, &[10, 30]),
            SlashResyncPlan::Defer
        );
        match app.plan_slash_resync(remote, &[10, 20, 30]) {
            SlashResyncPlan::Sync { guilds, invalidate } => {
                assert_eq!(guilds, vec![20]);
                assert!(!invalidate);
            }
            other => panic!("expected retry after guild returns, got {other:?}"),
        }

        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(format!("{}-wal", tmp.display()));
        let _ = std::fs::remove_file(format!("{}-shm", tmp.display()));
    }

    #[test]
    fn guild_page_cursor_stops_on_short_page() {
        fn g(id: &str) -> DiscordGuild {
            DiscordGuild {
                id: id.into(),
                name: "n".into(),
                icon: None,
                owner: false,
                permissions: None,
            }
        }
        let full: Vec<_> = (0..200).map(|i| g(&i.to_string())).collect();
        assert_eq!(next_guilds_after_cursor(&full, 200).as_deref(), Some("199"));
        let short = vec![g("200"), g("201")];
        assert_eq!(next_guilds_after_cursor(&short, 200), None);
        assert_eq!(next_guilds_after_cursor(&[], 200), None);

        // Simulate two-page merge the way my_guilds does.
        let mut all = Vec::new();
        let mut after = next_guilds_after_cursor(&full, 200);
        all.extend(full);
        assert_eq!(after.as_deref(), Some("199"));
        let page2 = vec![g("200")];
        after = next_guilds_after_cursor(&page2, 200);
        all.extend(page2);
        assert!(after.is_none());
        assert_eq!(all.len(), 201);
        assert_eq!(all.last().unwrap().id, "200");
    }

    #[test]
    fn guild_permissions_accepts_integer_and_string() {
        // Discord actually returns an integer here (docs say string).
        let g: DiscordGuild = serde_json::from_str(
            r#"{"id":"1","name":"x","icon":null,"owner":false,"permissions":32}"#,
        )
        .unwrap();
        assert!(g.manage_permission(), "MANAGE_GUILD (32) set");
        let g: DiscordGuild = serde_json::from_str(
            r#"{"id":"1","name":"x","icon":null,"owner":false,"permissions":"32"}"#,
        )
        .unwrap();
        assert!(g.manage_permission(), "string form works too");
        let g: DiscordGuild = serde_json::from_str(
            r#"{"id":"1","name":"x","icon":null,"owner":false,"permissions":1}"#,
        )
        .unwrap();
        assert!(!g.manage_permission(), "no MANAGE_GUILD");
        let g: DiscordGuild = serde_json::from_str(
            r#"{"id":"1","name":"x","icon":null,"owner":true,"permissions":0}"#,
        )
        .unwrap();
        assert!(g.manage_permission(), "owner bypass");
    }

    #[test]
    fn ban_permission_is_not_implied_by_manage_guild() {
        let manage_only: DiscordGuild = serde_json::from_str(
            r#"{"id":"1","name":"x","icon":null,"owner":false,"permissions":32}"#,
        )
        .unwrap();
        assert!(manage_only.manage_permission());
        assert!(!manage_only.ban_members_permission());

        let ban: DiscordGuild = serde_json::from_str(
            r#"{"id":"1","name":"x","icon":null,"owner":false,"permissions":4}"#,
        )
        .unwrap();
        assert!(ban.ban_members_permission());
    }
}
