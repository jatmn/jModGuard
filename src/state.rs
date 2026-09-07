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
    pub log_delivery_slots: tokio::sync::Semaphore,
    pub log_delivery_failures: AtomicU64,
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
    last_config_revision: parking_lot::Mutex<String>,
    /// Last observed durable `slash_registry_version` (commands table changes).
    /// Bot re-syncs Discord slash commands only when this advances — not on
    /// every generic `config_version` bump (rules/templates/settings/…).
    pub last_slash_registry_version: AtomicU64,
    last_slash_registry_revision: parking_lot::Mutex<String>,
    force_slash_registry_retry: AtomicBool,
    /// Serializes replica observation plus any primary confirmation. Config
    /// and slash pollers run independently, but an older primary snapshot must
    /// never reconcile after and overwrite a newer one.
    version_reconcile_lock: tokio::sync::Mutex<()>,
    /// Per-target retry state for slash re-sync (see [`SlashSyncProgress`]).
    slash_sync_progress: Mutex<Option<SlashSyncProgress>>,
    /// Per-user OAuth guild-list cache (discord_user_id -> (fetched_at, guilds)).
    pub guild_cache: DashMap<u64, (Instant, Vec<DiscordGuild>)>,
    pub user_cache: DashMap<u64, (Instant, DiscordUser)>,
}

impl AppState {
    pub fn new(cfg: Config, db: Db) -> Result<Arc<Self>> {
        anyhow::ensure!(
            cfg.log.discord_concurrency > 0
                && cfg.log.discord_concurrency <= tokio::sync::Semaphore::MAX_PERMITS,
            "log.discord_concurrency must be positive and within the runtime semaphore limit"
        );
        anyhow::ensure!(
            cfg.log.discord_timeout_secs > 0
                && Instant::now()
                    .checked_add(Duration::from_secs(cfg.log.discord_timeout_secs))
                    .is_some(),
            "log.discord_timeout_secs must be positive"
        );
        let log_delivery_slots = tokio::sync::Semaphore::new(cfg.log.discord_concurrency);
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
        let primed = db::get_primary_versions_sync(&db)
            .context("failed to read primary version baselines while building AppState")?;
        db.require_config_version(primed.config);
        Ok(Arc::new(Self {
            db,
            cfg: Arc::new(cfg),
            configs: ConfigCache::new(primed.config, primed.config_revision.clone()),
            automod: AutomodEngine::new(),
            cooldowns: DashMap::new(),
            log_delivery_slots,
            log_delivery_failures: AtomicU64::new(0),
            oauth,
            http,
            discord_http_cell: std::sync::OnceLock::new(),
            bot_user_id: tokio::sync::OnceCell::new(),
            start: Instant::now(),
            slash_synced: AtomicBool::new(false),
            last_config_version: AtomicU64::new(primed.config),
            last_config_revision: parking_lot::Mutex::new(primed.config_revision),
            last_slash_registry_version: AtomicU64::new(primed.slash),
            last_slash_registry_revision: parking_lot::Mutex::new(primed.slash_revision),
            force_slash_registry_retry: AtomicBool::new(false),
            version_reconcile_lock: tokio::sync::Mutex::new(()),
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
                let me = self.discord_http();
                let me = tokio::time::timeout(
                    crate::discord::DISCORD_REST_TIMEOUT,
                    me.get_current_user(),
                )
                .await
                .context("timed out fetching bot user id from Discord")?
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
        let _version_reconcile = self.version_reconcile_lock.lock().await;
        let remote = db::get_version_state(&self.db).await?;
        let prev = self.last_config_version.load(Ordering::SeqCst);
        let revision_changed = remote.config_revision != *self.last_config_revision.lock();
        if revision_changed
            || remote.config != prev
            || remote.config < self.db.required_config_version()
        {
            let suspected_floor = prev.max(self.db.required_config_version());
            let write_generation = self.db.config_write_generation();
            let primary = db::get_primary_versions(&self.db).await?;
            return Ok(self
                .reconcile_primary_versions(primary, suspected_floor, write_generation)
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
        let _version_reconcile = self.version_reconcile_lock.lock().await;
        let remote = db::get_version_state(&self.db).await?;
        let prev = self.last_slash_registry_version.load(Ordering::SeqCst);
        let config_revision_changed = remote.config_revision != *self.last_config_revision.lock();
        let slash_revision_changed =
            remote.slash_revision != *self.last_slash_registry_revision.lock();
        if config_revision_changed
            || slash_revision_changed
            || remote.config < self.db.required_config_version()
            || remote.slash != prev
        {
            let suspected_floor = self
                .last_config_version
                .load(Ordering::SeqCst)
                .max(self.db.required_config_version());
            let write_generation = self.db.config_write_generation();
            let primary = db::get_primary_versions(&self.db).await?;
            let (_, slash_changed) =
                self.reconcile_primary_versions(primary, suspected_floor, write_generation);
            let current = self.last_slash_registry_version.load(Ordering::SeqCst);
            Ok(
                (slash_changed || self.force_slash_registry_retry.load(Ordering::SeqCst))
                    .then_some(current),
            )
        } else if self.force_slash_registry_retry.load(Ordering::SeqCst) {
            Ok(Some(prev))
        } else {
            Ok(None)
        }
    }

    fn reconcile_primary_versions(
        &self,
        primary: db::VersionState,
        suspected_floor: u64,
        write_generation: u64,
    ) -> (bool, bool) {
        if !self.db.rebaseline_required_config_version(
            suspected_floor,
            primary.config,
            write_generation,
        ) {
            tracing::debug!(
                "discarding primary version snapshot because a local config write committed concurrently"
            );
            return (false, false);
        }
        let previous_config = self.last_config_version.load(Ordering::SeqCst);
        if primary.config < suspected_floor {
            tracing::warn!(
                previous_config,
                required_config = suspected_floor,
                primary_config = primary.config,
                "primary database confirmed config version below the local floor; rebaselining"
            );
            self.configs
                .rebaseline_version(suspected_floor, primary.config);
        }
        let config_revision_changed = {
            let mut revision = self.last_config_revision.lock();
            if *revision != primary.config_revision {
                *revision = primary.config_revision.clone();
                true
            } else {
                false
            }
        };
        let config_changed = config_revision_changed || primary.config != previous_config;
        if config_changed {
            self.last_config_version
                .store(primary.config, Ordering::SeqCst);
            if config_revision_changed {
                // Snapshot acceptance carries the revision identity, so a
                // higher pre-failover replica counter cannot bypass rollback
                // recovery and later invalidations cannot forget lineage.
                self.configs
                    .require_primary_revision(primary.config, primary.config_revision.clone());
            } else if primary.config >= previous_config {
                self.configs.require_version(primary.config);
            } else {
                self.configs.invalidate();
            }
        }

        let previous_slash = self.last_slash_registry_version.load(Ordering::SeqCst);
        let slash_revision_changed = {
            let mut revision = self.last_slash_registry_revision.lock();
            if *revision != primary.slash_revision {
                *revision = primary.slash_revision;
                true
            } else {
                false
            }
        };
        let slash_changed = slash_revision_changed || primary.slash != previous_slash;
        if slash_changed {
            self.last_slash_registry_version
                .store(primary.slash, Ordering::SeqCst);
            if primary.slash < previous_slash {
                tracing::warn!(
                    previous_slash,
                    primary_slash = primary.slash,
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
        let mut progress = self.slash_sync_progress.lock();
        self.force_slash_registry_retry
            .store(true, Ordering::SeqCst);
        *progress = None;
    }

    pub fn acknowledged_slash_registry_version(&self) -> u64 {
        self.last_slash_registry_version.load(Ordering::SeqCst)
    }

    pub fn slash_registry_revision(&self) -> String {
        self.last_slash_registry_revision.lock().clone()
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
                if pending.len() != rem.len() {
                    // Do not partially consume the failure set: otherwise a
                    // successful present guild could acknowledge the version
                    // while another failed guild remains absent from cache.
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

    /// Record guilds that failed `set_commands` for `remote`. A successful
    /// finish commits the version and clears the force flag while holding the
    /// progress lock, so a concurrent retry request cannot be erased.
    pub fn finish_slash_resync(&self, remote: u64, failed: HashSet<u64>) -> bool {
        let mut guard = self.slash_sync_progress.lock();
        let Some(progress) = guard.as_mut() else {
            // A concurrent guild-create/startup failure clears progress to
            // request a fresh attempt. Never let an older in-flight poll
            // acknowledge and erase that newer retry request.
            return false;
        };
        if progress.target != remote {
            return false;
        }
        if failed.is_empty() {
            *guard = None;
            self.last_slash_registry_version
                .store(remote, Ordering::SeqCst);
            self.force_slash_registry_retry
                .store(false, Ordering::SeqCst);
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
    publication_lock: Arc<parking_lot::Mutex<()>>,
    accepted_revision: Arc<parking_lot::Mutex<String>>,
}

impl Default for ConfigCache {
    fn default() -> Self {
        Self::new(0, "legacy:0".into())
    }
}

impl ConfigCache {
    pub fn new(durable_floor: u64, revision: String) -> Self {
        Self {
            map: DashMap::new(),
            version: Arc::new(AtomicU64::new(0)),
            ttl: Duration::from_secs(15),
            durable_floor: Arc::new(AtomicU64::new(durable_floor)),
            publication_lock: Arc::new(parking_lot::Mutex::new(())),
            accepted_revision: Arc::new(parking_lot::Mutex::new(revision)),
        }
    }

    pub fn invalidate(&self) {
        let _publication = self.publication_lock.lock();
        self.version.fetch_add(1, Ordering::Relaxed);
        self.map.clear();
    }

    pub fn require_version(&self, version: u64) {
        self.durable_floor.fetch_max(version, Ordering::SeqCst);
        self.invalidate();
    }

    fn require_primary_revision(&self, version: u64, revision: String) {
        let _publication = self.publication_lock.lock();
        self.durable_floor.store(version, Ordering::SeqCst);
        *self.accepted_revision.lock() = revision;
        self.version.fetch_add(1, Ordering::Relaxed);
        self.map.clear();
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
        let mut primary = false;
        let mut last_observed = 0;
        let mut last_required = required;
        for _ in 0..3 {
            let version = self.version.load(Ordering::Relaxed);
            let (durable_version, revision, parts) = if primary {
                db::load_config_parts_with_version_primary(db, guild_id as i64).await?
            } else {
                db::load_config_parts_with_version(db, guild_id as i64).await?
            };
            let required_after = self
                .durable_floor
                .load(Ordering::SeqCst)
                .max(db.required_config_version());
            last_observed = durable_version;
            last_required = required_after;
            let accepted_revision = self.accepted_revision.lock().clone();

            if primary && revision != accepted_revision {
                self.require_primary_revision(durable_version, revision);
                continue;
            }
            if durable_version < required_after || revision != accepted_revision {
                // Replica lag is expected after a successful remote write. Do
                // not drop moderation/commands while waiting for convergence:
                // retry through one primary-routed, single-snapshot read.
                primary = true;
                continue;
            }
            let cfg = Arc::new(CachedGuildConfig::from_parts(
                version,
                durable_version,
                parts,
            ));
            {
                // Publication and invalidation share one lock, closing the
                // check→insert window that could republish a stale snapshot
                // after a same-process write had cleared the cache.
                let _publication = self.publication_lock.lock();
                let required_now = self
                    .durable_floor
                    .load(Ordering::SeqCst)
                    .max(db.required_config_version());
                let accepted_revision = self.accepted_revision.lock().clone();
                if self.version.load(Ordering::Relaxed) != version
                    || durable_version < required_now
                    || revision != accepted_revision
                {
                    primary = true;
                    continue;
                }
                self.map.insert(guild_id as i64, Arc::clone(&cfg));
                return Ok(cfg);
            }
        }
        anyhow::bail!(
            "database config changed repeatedly while loading (version {last_observed}, expected at least {last_required})"
        )
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
    fn revision_token_detects_failover_aba_at_same_version() {
        let tmp = std::env::temp_dir().join(format!(
            "jmodguard_revision_aba_{}_{}.db",
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
        let guild_id = 123u64;
        let baseline = bot.last_config_version.load(Ordering::SeqCst);
        assert_eq!(
            tokio_test_block_on(bot.configs.get(&bot.db, guild_id))
                .unwrap()
                .settings
                .prefix,
            "!"
        );

        tokio_test_block_on(db::patch_settings(
            &api_db,
            guild_id as i64,
            Some(Some("aba-visible".into())),
            None,
            None,
        ))
        .unwrap();
        tokio_test_block_on(api_db.with_conn(move |conn| {
            let tx = conn.transaction_immediate()?;
            tx.execute_batch(&format!(
                "UPDATE meta SET value = '{baseline}' WHERE key = 'config_version'"
            ))?;
            tx.commit()
        }))
        .unwrap();

        assert!(
            tokio_test_block_on(bot.poll_config_version()).unwrap(),
            "a new revision must invalidate even when the restored counter reuses the old value"
        );
        assert_eq!(
            tokio_test_block_on(bot.configs.get(&bot.db, guild_id))
                .unwrap()
                .settings
                .prefix,
            "aba-visible"
        );

        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(format!("{}-wal", tmp.display()));
        let _ = std::fs::remove_file(format!("{}-shm", tmp.display()));
    }

    #[test]
    fn slash_revision_forces_sync_when_counter_is_reused() {
        let tmp = std::env::temp_dir().join(format!(
            "jmodguard_slash_revision_aba_{}_{}.db",
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
        let versions = db::get_primary_versions_sync(&app.db).unwrap();
        let before_write = app.db.config_write_generation();
        let replacement = "replacement-slash-revision".to_string();
        let mut restored = versions.clone();
        restored.slash_revision = replacement.clone();

        assert_eq!(
            app.reconcile_primary_versions(restored, versions.config, before_write,),
            (false, true)
        );
        assert_eq!(app.slash_registry_revision(), replacement);
        assert!(app.force_slash_registry_retry.load(Ordering::SeqCst));

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
    fn primary_rebaseline_rejects_snapshot_across_concurrent_local_write() {
        let tmp = std::env::temp_dir().join(format!(
            "jmodguard_rebaseline_generation_{}_{}.db",
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
        app.db.require_config_version(11);
        app.configs.require_version(11);
        app.last_config_version.store(11, Ordering::SeqCst);
        let before_write = app.db.config_write_generation();

        tokio_test_block_on(db::patch_settings(
            &app.db,
            77,
            Some(Some("reused-version-write".into())),
            None,
            None,
        ))
        .unwrap();

        assert_eq!(
            app.reconcile_primary_versions(
                db::VersionState {
                    config: 9,
                    slash: 9,
                    config_revision: "rollback-config".into(),
                    slash_revision: "rollback-slash".into(),
                },
                11,
                before_write,
            ),
            (false, false),
            "a primary snapshot taken before a concurrent local commit must be discarded"
        );
        assert_eq!(app.db.required_config_version(), 11);
        assert_eq!(app.last_config_version.load(Ordering::SeqCst), 11);

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

        // A concurrent guild-create failure requests a fresh pass while this
        // one is in flight. The older successful pass must not erase it.
        app.retry_slash_registry_version();
        assert!(!app.finish_slash_resync(remote, HashSet::new()));
        match app.plan_slash_resync(remote, &[10, 20, 30, 40]) {
            SlashResyncPlan::Sync { guilds, .. } => assert_eq!(guilds, vec![10, 20, 30, 40]),
            other => panic!("expected fresh retry plan, got {other:?}"),
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
        assert_eq!(
            app.plan_slash_resync(remote, &[10, 20, 30]),
            SlashResyncPlan::Sync {
                guilds: vec![20],
                invalidate: false,
            }
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
