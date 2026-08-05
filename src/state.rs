//! Shared application state: DB, config, automod engine, OAuth client,
//! and the per-guild config cache (with global fallback + invalidation).

use anyhow::{Context as _, Result};
use dashmap::DashMap;
use regex::Regex;
use serenity::model::id::ChannelId;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::automod::AutomodEngine;
use crate::config::Config;
use crate::db::{self, Db};
use crate::types::{CommandDef, GuildSettings, RuleDef};

pub struct AppState {
    pub db: Db,
    pub cfg: Arc<Config>,
    pub configs: ConfigCache,
    pub automod: AutomodEngine,
    pub cooldowns: DashMap<(u64, u64, String), Instant>,
    #[allow(dead_code)]
    pub http: reqwest::Client,
    pub oauth: OAuthClient,
    pub start: Instant,
    pub slash_synced: AtomicBool,
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
        Ok(Arc::new(Self {
            db,
            cfg: Arc::new(cfg),
            configs: ConfigCache::new(),
            automod: AutomodEngine::new(),
            cooldowns: DashMap::new(),
            oauth,
            http,
            start: Instant::now(),
            slash_synced: AtomicBool::new(false),
            guild_cache: DashMap::new(),
            user_cache: DashMap::new(),
        }))
    }

    /// Invalidates the config cache (call after any config write).
    pub fn invalidate_configs(&self) {
        self.configs.invalidate();
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
        self.permissions.as_ref().map(|p| p.as_u64() & MANAGE_GUILD != 0).unwrap_or(false)
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
            self.client_id,
            redirect,
            "identify%20guilds",
            state
        )
    }

    pub async fn exchange_code(&self, code: &str) -> Result<TokenResponse> {
        self.token_request("authorization_code", code, None).await
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<TokenResponse> {
        self.token_request("refresh_token", refresh_token, None).await
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
                    if grant_type == "authorization_code" { "code" } else { "refresh_token" },
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
        let resp = self
            .http
            .get("https://discord.com/api/users/@me/guilds")
            .bearer_auth(access_token)
            .send()
            .await
            .context("discord guilds request failed")?;
        if !resp.status().is_success() {
            anyhow::bail!("discord guilds request failed with {}", resp.status());
        }
        resp.json().await.context("invalid discord guilds response")
    }
}

// ---------------------------------------------------------------------------
// per-guild config cache
// ---------------------------------------------------------------------------

pub struct CachedGuildConfig {
    pub version: u64,
    pub loaded_at: Instant,
    pub settings: GuildSettings,
    pub rules: HashMap<String, RuleDef>,
    pub commands: HashMap<String, CommandDef>,
    pub templates: HashMap<String, String>,
    pub log_channels: HashMap<String, ChannelId>,
    pub regexes: Vec<Regex>,
}

impl CachedGuildConfig {
    fn from_parts(version: u64, parts: db::ConfigParts) -> Self {
        let mut regexes = Vec::new();
        for pattern in &parts.badwords {
            match Regex::new(pattern) {
                Ok(re) => regexes.push(re),
                Err(e) => {
                    tracing::warn!("badword pattern `{pattern}` failed to compile: {e}");
                }
            }
        }
        Self {
            version,
            loaded_at: Instant::now(),
            settings: parts.settings,
            rules: parts.rules,
            commands: parts.commands,
            templates: parts.templates,
            log_channels: parts
                .log_channels
                .into_iter()
                .map(|(k, v)| (k, ChannelId::new(v as u64)))
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
}

impl Default for ConfigCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigCache {
    pub fn new() -> Self {        Self {
            map: DashMap::new(),
            version: Arc::new(AtomicU64::new(0)),
            ttl: Duration::from_secs(15),
        }
    }

    pub fn invalidate(&self) {
        self.version.fetch_add(1, Ordering::Relaxed);
        self.map.clear();
    }

    pub async fn get(&self, db: &Db, guild_id: u64) -> Result<Arc<CachedGuildConfig>> {
        if let Some(entry) = self.map.get(&(guild_id as i64))
            && entry.version == self.version.load(Ordering::Relaxed)
                && entry.loaded_at.elapsed() < self.ttl
            {
                return Ok(Arc::clone(&entry));
            }
        let version = self.version.load(Ordering::Relaxed);
        let parts = db::load_config_parts(db, guild_id as i64).await?;
        let cfg = Arc::new(CachedGuildConfig::from_parts(version, parts));
        self.map.insert(guild_id as i64, Arc::clone(&cfg));
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guild_permissions_accepts_integer_and_string() {
        // Discord actually returns an integer here (docs say string).
        let g: DiscordGuild =
            serde_json::from_str(r#"{"id":"1","name":"x","icon":null,"owner":false,"permissions":32}"#)
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
        let g: DiscordGuild =
            serde_json::from_str(r#"{"id":"1","name":"x","icon":null,"owner":true,"permissions":0}"#)
                .unwrap();
        assert!(g.manage_permission(), "owner bypass");
    }
}
