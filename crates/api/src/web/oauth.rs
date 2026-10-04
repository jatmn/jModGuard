//! Discord OAuth client and dashboard identity models.
use anyhow::{Context as _, Result};
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub struct OAuthClient {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    pub http: reqwest::Client,
    /// Route cooldown map. Shared so a dropped request can still store the body wait.
    pub(crate) guild_list: std::sync::Arc<std::sync::Mutex<GuildListGate>>,
    /// One in-flight `/users/@me/guilds` lookup per user. Waiters receive the leader's result.
    guild_flights: std::sync::Arc<std::sync::Mutex<HashMap<String, GuildFlightSender>>>,
    /// Core database used to keep Retry-After across process restarts. Empty in tests
    /// that do not open a database.
    retry_db: std::sync::Mutex<Option<crate::db::Db>>,
    #[cfg(test)]
    pub guilds_endpoint: Option<String>,
    #[cfg(test)]
    pub token_endpoint: Option<String>,
    #[cfg(test)]
    pub me_endpoint: Option<String>,
}

type GuildFlightSender = tokio::sync::watch::Sender<Option<std::sync::Arc<SharedGuildOutcome>>>;

pub(crate) struct GuildListGate {
    /// Route key → earliest instant another request for that route may start.
    cooldowns: HashMap<String, Cooldown>,
    /// Durable unix-millis deadlines. `Instant` values do not survive a restart.
    deadlines_ms: HashMap<String, i64>,
    generation: u64,
}

/// A cooldown that cannot be added to `Instant` stays until this process exits,
/// so a later guild-list call cannot start early.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cooldown {
    Until(Instant),
    RestOfProcess,
    /// Discord already returned 429. Stay blocked until its body is applied.
    AwaitingBody,
}

impl GuildListGate {
    pub(crate) fn new() -> Self {
        Self {
            cooldowns: HashMap::new(),
            deadlines_ms: HashMap::new(),
            generation: 0,
        }
    }

    fn remaining(&mut self, token: &str) -> Option<Duration> {
        let now = Instant::now();
        self.cooldowns.retain(|_, cooldown| match *cooldown {
            Cooldown::RestOfProcess | Cooldown::AwaitingBody => true,
            Cooldown::Until(until) => until > now,
        });
        match self.cooldowns.get(token).copied()? {
            Cooldown::RestOfProcess | Cooldown::AwaitingBody => Some(Duration::MAX),
            Cooldown::Until(until) => Some(until.saturating_duration_since(now)),
        }
    }

    fn block_until(&mut self, token: &str, retry_after: Duration) {
        let next = cooldown_deadline(Instant::now(), retry_after);
        let replace = match self.cooldowns.get(token).copied() {
            Some(Cooldown::RestOfProcess) => false,
            Some(Cooldown::AwaitingBody) => true,
            Some(Cooldown::Until(existing)) => match next {
                Cooldown::RestOfProcess => true,
                Cooldown::Until(until) => until > existing,
                Cooldown::AwaitingBody => true,
            },
            None => true,
        };
        if replace {
            self.cooldowns.insert(token.to_owned(), next);
            if super::discord_retry::durable_key(token) {
                let millis = wall_deadline_ms(retry_after);
                let previous = self.deadlines_ms.get(token).copied().unwrap_or(0);
                if millis > previous {
                    self.deadlines_ms.insert(token.to_owned(), millis);
                    self.generation = self.generation.saturating_add(1);
                }
            }
        }
    }

    fn durable_snapshot(&mut self) -> (u64, std::collections::BTreeMap<String, i64>) {
        let now = chrono::Utc::now().timestamp_millis();
        self.deadlines_ms
            .retain(|key, millis| *millis > now && super::discord_retry::durable_key(key));
        while self.deadlines_ms.len() > 64 {
            let Some(soonest) = self
                .deadlines_ms
                .iter()
                .min_by_key(|(_, millis)| *millis)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.deadlines_ms.remove(&soonest);
        }
        (
            self.generation,
            self.deadlines_ms
                .iter()
                .map(|(key, millis)| (key.clone(), *millis))
                .collect(),
        )
    }
}

fn wall_deadline_ms(retry_after: Duration) -> i64 {
    let now = chrono::Utc::now().timestamp_millis();
    let extra = i64::try_from(retry_after.as_millis().min(i64::MAX as u128)).unwrap_or(i64::MAX);
    now.saturating_add(extra)
}

pub(crate) fn cooldown_deadline(now: Instant, retry_after: Duration) -> Cooldown {
    match now.checked_add(retry_after) {
        Some(until) => Cooldown::Until(until),
        None => Cooldown::RestOfProcess,
    }
}

/// Upstream guild-list failure. Only `Unauthorized` justifies an OAuth refresh.
#[derive(Debug)]
pub enum GuildListError {
    /// `fresh` is the Discord 429 itself. A later call inside that window is not.
    RateLimited {
        retry_after: Duration,
        fresh: bool,
    },
    Unauthorized,
    Failed(anyhow::Error),
}

/// Browser-facing temporary failure. Not an authentication or client error.
#[derive(Debug)]
pub struct DiscordRateLimit {
    pub retry_after: Duration,
}

impl std::fmt::Display for DiscordRateLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let seconds = retry_after_header_secs(self.retry_after);
        write!(
            f,
            "Discord is temporarily rate limiting requests. Try again in about {seconds}s."
        )
    }
}

impl std::error::Error for DiscordRateLimit {}

/// Seconds advertised to the browser. Missing or zero Discord values still
/// impose a one-second floor so callers do not hammer immediately. The HTTP
/// handler returns immediately; this number is not a sleep.
pub(crate) fn retry_after_header_secs(retry_after: Duration) -> u64 {
    let seconds = retry_after.as_millis().div_ceil(1000);
    u64::try_from(seconds).unwrap_or(u64::MAX).max(1)
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

    pub fn administrator_permission(&self) -> bool {
        const ADMINISTRATOR: u64 = 1 << 3;
        self.owner
            || self
                .permissions
                .as_ref()
                .map(|p| p.as_u64() & ADMINISTRATOR != 0)
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
        if let Some(retry_after) = self.route_remaining("token") {
            return Err(DiscordRateLimit { retry_after }.into());
        }
        let url = "https://discord.com/api/oauth2/token";
        #[cfg(test)]
        let url = self.token_endpoint.as_deref().unwrap_or(url);
        let resp = self
            .http
            .post(url)
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
        let headers = resp.headers().clone();
        let bytes = resp.bytes().await?;
        self.observe_route("token", status, &headers, &bytes).await;
        if status.as_u16() == 429 {
            return Err(DiscordRateLimit {
                retry_after: discord_retry_after(header_retry_after(&headers), &bytes),
            }
            .into());
        }
        if !status.is_success() {
            anyhow::bail!(
                "discord oauth error {status}: {}",
                String::from_utf8_lossy(&bytes)
            );
        }
        serde_json::from_slice(&bytes).context("invalid oauth token response")
    }

    pub async fn me(&self, user_id: u64, access_token: &str) -> Result<DiscordUser> {
        let key = if user_id == 0 {
            format!("m:login:{:016x}", token_fingerprint(access_token))
        } else {
            format!("m:{user_id}")
        };
        if let Some(retry_after) = self.route_remaining(&key) {
            return Err(DiscordRateLimit { retry_after }.into());
        }
        let url = "https://discord.com/api/users/@me";
        #[cfg(test)]
        let url = self.me_endpoint.as_deref().unwrap_or(url);
        let resp = self
            .http
            .get(url)
            .bearer_auth(access_token)
            .send()
            .await
            .context("discord me request failed")?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp
            .bytes()
            .await
            .context("discord me response body failed")?;
        self.observe_route(&key, status, &headers, &bytes).await;
        if status.as_u16() == 429 {
            return Err(DiscordRateLimit {
                retry_after: discord_retry_after(header_retry_after(&headers), &bytes),
            }
            .into());
        }
        if !status.is_success() {
            anyhow::bail!("discord me request failed with {status}");
        }
        serde_json::from_slice(&bytes).context("invalid discord user response")
    }

    pub async fn my_guilds(
        &self,
        user_id: u64,
        access_token: &str,
    ) -> std::result::Result<Vec<DiscordGuild>, GuildListError> {
        let user_key = format!("g:{user_id}");
        if let Some(retry_after) = self.route_remaining(&user_key) {
            return Err(GuildListError::RateLimited {
                retry_after,
                fresh: false,
            });
        }
        // One lookup per user. A different user does not wait, and a refreshed
        // access token does not start a second request for this user.
        match self.begin_flight(&user_key) {
            FlightJoin::Follow(receiver) => self.wait_for_flight(receiver).await,
            FlightJoin::Lead(mut guard) => {
                if let Some(retry_after) = self.route_remaining(&user_key) {
                    let error = GuildListError::RateLimited {
                        retry_after,
                        fresh: false,
                    };
                    guard.finish(&error_outcome(&error));
                    return Err(error);
                }
                let result = self.fetch_guild_pages(&user_key, access_token).await;
                guard.finish(&shared_outcome(&result));
                if matches!(result, Err(GuildListError::RateLimited { fresh: true, .. })) {
                    self.persist_deadlines().await;
                }
                result
            }
        }
    }

    async fn fetch_guild_pages(
        &self,
        user_key: &str,
        access_token: &str,
    ) -> std::result::Result<Vec<DiscordGuild>, GuildListError> {
        // Discord paginates at 200 guilds; a single GET silently drops every
        // server past the first page and denies WebUI manage access for them.
        const PAGE_LIMIT: usize = 200;
        let endpoint = "https://discord.com/api/users/@me/guilds";
        #[cfg(test)]
        let endpoint = self.guilds_endpoint.as_deref().unwrap_or(endpoint);
        let mut all = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let mut url = format!("{endpoint}?limit={PAGE_LIMIT}");
            if let Some(ref cursor) = after {
                url.push_str("&after=");
                url.push_str(cursor);
            }
            let resp = match self.http.get(&url).bearer_auth(access_token).send().await {
                Ok(resp) => resp,
                Err(error) => {
                    return Err(GuildListError::Failed(anyhow::anyhow!(
                        "discord guilds request failed: {error}"
                    )));
                }
            };
            let status = resp.status();
            let headers = resp.headers().clone();
            if status.as_u16() == 401 {
                return Err(GuildListError::Unauthorized);
            }
            if status.as_u16() == 429 {
                let header = header_retry_after(&headers).map(str::to_owned);
                let header_global = limit_scope_is_global(&headers);
                let gate = std::sync::Arc::clone(&self.guild_list);
                let cooldown_key = user_key.to_owned();
                self.hold_for_body(user_key);
                let header_for_response = header.clone();
                let finish = tokio::spawn(async move {
                    let (retry_after, body_global) = match resp.bytes().await {
                        Ok(bytes) => (
                            discord_retry_after(header.as_deref(), &bytes),
                            body_is_global(&bytes),
                        ),
                        Err(_) if header.is_some() => {
                            (discord_retry_after(header.as_deref(), b"{}"), false)
                        }
                        Err(_) => {
                            gate.lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .block_until(&cooldown_key, Duration::MAX);
                            return Duration::MAX;
                        }
                    };
                    let mut gate = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    if header_global || body_global {
                        gate.block_until("global", retry_after);
                    }
                    gate.block_until(&cooldown_key, retry_after);
                    retry_after
                });
                let retry_after = finish
                    .await
                    .unwrap_or_else(|_| discord_retry_after(header_for_response.as_deref(), b"{}"));
                return Err(GuildListError::RateLimited {
                    retry_after,
                    fresh: true,
                });
            }
            if !status.is_success() {
                return Err(GuildListError::Failed(anyhow::anyhow!(
                    "discord guilds request failed with {status}"
                )));
            }
            let bytes = match resp.bytes().await {
                Ok(bytes) => bytes,
                Err(error) => {
                    return Err(GuildListError::Failed(anyhow::anyhow!(
                        "invalid discord guilds response: {error}"
                    )));
                }
            };
            if self.note_bucket_reset(user_key, &headers, &bytes) {
                self.persist_deadlines().await;
            }
            let page: Vec<DiscordGuild> = match serde_json::from_slice(&bytes) {
                Ok(page) => page,
                Err(error) => {
                    return Err(GuildListError::Failed(anyhow::anyhow!(
                        "invalid discord guilds response: {error}"
                    )));
                }
            };
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

    fn route_remaining(&self, key: &str) -> Option<Duration> {
        let mut gate = self
            .guild_list
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let global = gate.remaining("global");
        let route = if key == "global" {
            None
        } else {
            gate.remaining(key)
        };
        match (global, route) {
            (Some(global), Some(route)) => Some(global.max(route)),
            (Some(wait), None) | (None, Some(wait)) => Some(wait),
            (None, None) => None,
        }
    }

    /// True when this user's guild list, or the process-wide Discord limit, is still closed.
    pub(crate) fn guild_route_limited(&self, user_id: u64) -> bool {
        self.route_remaining(&format!("g:{user_id}")).is_some()
    }

    fn hold_for_body(&self, user_key: &str) {
        let mut gate = self
            .guild_list
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !matches!(
            gate.cooldowns.get(user_key),
            Some(Cooldown::RestOfProcess | Cooldown::Until(_))
        ) {
            gate.cooldowns
                .insert(user_key.to_owned(), Cooldown::AwaitingBody);
        }
    }

    fn begin_flight(&self, user_key: &str) -> FlightJoin {
        let mut flights = self
            .guild_flights
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(sender) = flights.get(user_key) {
            return FlightJoin::Follow(sender.subscribe());
        }
        let (sender, _) = tokio::sync::watch::channel(None);
        flights.insert(user_key.to_owned(), sender.clone());
        FlightJoin::Lead(FlightGuard {
            key: user_key.to_owned(),
            sender,
            flights: std::sync::Arc::clone(&self.guild_flights),
            done: false,
        })
    }

    async fn wait_for_flight(
        &self,
        mut receiver: tokio::sync::watch::Receiver<Option<std::sync::Arc<SharedGuildOutcome>>>,
    ) -> std::result::Result<Vec<DiscordGuild>, GuildListError> {
        loop {
            if let Some(outcome) = receiver.borrow().clone() {
                return outcome_to_result(&outcome);
            }
            if receiver.changed().await.is_err() {
                return Err(GuildListError::Failed(anyhow::anyhow!(
                    "guild list lookup ended before a result"
                )));
            }
        }
    }

    async fn observe_route(
        &self,
        key: &str,
        status: reqwest::StatusCode,
        headers: &reqwest::header::HeaderMap,
        body: &[u8],
    ) {
        if status.as_u16() == 429 {
            let retry_after = discord_retry_after(header_retry_after(headers), body);
            self.apply_limit(key, retry_after);
            if limit_scope_is_global(headers) || body_is_global(body) {
                self.apply_limit("global", retry_after);
            }
            self.persist_deadlines().await;
            return;
        }
        if self.note_bucket_reset(key, headers, body) {
            self.persist_deadlines().await;
        }
    }

    fn note_bucket_reset(
        &self,
        key: &str,
        headers: &reqwest::header::HeaderMap,
        body: &[u8],
    ) -> bool {
        let Some(retry_after) = bucket_reset_after(headers) else {
            return false;
        };
        self.apply_limit(key, retry_after);
        if limit_scope_is_global(headers) || body_is_global(body) {
            self.apply_limit("global", retry_after);
        }
        true
    }

    fn apply_limit(&self, key: &str, retry_after: Duration) {
        self.guild_list
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .block_until(key, retry_after);
    }

    pub(crate) fn restore_deadlines(&self, deadlines: std::collections::BTreeMap<String, i64>) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let now = Instant::now();
        let mut gate = self
            .guild_list
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (key, millis) in deadlines {
            if millis <= now_ms || !super::discord_retry::durable_key(&key) {
                continue;
            }
            let retry_after =
                Duration::from_millis(u64::try_from(millis - now_ms).unwrap_or(u64::MAX));
            gate.cooldowns
                .insert(key.clone(), cooldown_deadline(now, retry_after));
            gate.deadlines_ms.insert(key, millis);
        }
    }

    pub(crate) fn attach_retry_store(&self, db: crate::db::Db) {
        *self
            .retry_db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(db);
    }

    async fn persist_deadlines(&self) {
        let Some(db) = self
            .retry_db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        else {
            return;
        };
        for _ in 0..4 {
            let (generation, deadlines) = self
                .guild_list
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .durable_snapshot();
            let Ok(json) = serde_json::to_string(&deadlines) else {
                return;
            };
            if json.len() > 8192 {
                tracing::warn!("discord retry deadline record is too large to store");
                return;
            }
            if let Err(error) = super::discord_retry::store_deadlines(&db, json).await {
                tracing::warn!("failed to persist discord retry deadline: {error:#}");
                return;
            }
            let current = self
                .guild_list
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .generation;
            if current == generation {
                return;
            }
        }
    }
}

enum FlightJoin {
    Lead(FlightGuard),
    Follow(tokio::sync::watch::Receiver<Option<std::sync::Arc<SharedGuildOutcome>>>),
}

struct FlightGuard {
    key: String,
    sender: GuildFlightSender,
    flights: std::sync::Arc<std::sync::Mutex<HashMap<String, GuildFlightSender>>>,
    done: bool,
}

impl FlightGuard {
    fn finish(&mut self, outcome: &SharedGuildOutcome) {
        let _ = self.sender.send(Some(std::sync::Arc::new(outcome.clone())));
        self.done = true;
    }
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        if !self.done {
            let _ = self
                .sender
                .send(Some(std::sync::Arc::new(SharedGuildOutcome::Failed(
                    "guild list lookup ended before a result".into(),
                ))));
        }
        let mut flights = self
            .flights
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        flights.remove(&self.key);
    }
}

#[derive(Clone)]
enum SharedGuildOutcome {
    Ok(Vec<DiscordGuild>),
    Limited { retry_after: Duration, fresh: bool },
    Unauthorized,
    Failed(String),
}

fn shared_outcome(
    result: &std::result::Result<Vec<DiscordGuild>, GuildListError>,
) -> SharedGuildOutcome {
    match result {
        Ok(guilds) => SharedGuildOutcome::Ok(guilds.clone()),
        Err(error) => error_outcome(error),
    }
}

fn error_outcome(error: &GuildListError) -> SharedGuildOutcome {
    match error {
        GuildListError::RateLimited { retry_after, fresh } => SharedGuildOutcome::Limited {
            retry_after: *retry_after,
            fresh: *fresh,
        },
        GuildListError::Unauthorized => SharedGuildOutcome::Unauthorized,
        GuildListError::Failed(error) => SharedGuildOutcome::Failed(format!("{error:#}")),
    }
}

fn outcome_to_result(
    outcome: &SharedGuildOutcome,
) -> std::result::Result<Vec<DiscordGuild>, GuildListError> {
    match outcome {
        SharedGuildOutcome::Ok(guilds) => Ok(guilds.clone()),
        SharedGuildOutcome::Limited { retry_after, fresh } => Err(GuildListError::RateLimited {
            retry_after: *retry_after,
            fresh: *fresh,
        }),
        SharedGuildOutcome::Unauthorized => Err(GuildListError::Unauthorized),
        SharedGuildOutcome::Failed(message) => {
            Err(GuildListError::Failed(anyhow::anyhow!(message.clone())))
        }
    }
}

fn header_retry_after(headers: &reqwest::header::HeaderMap) -> Option<&str> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
}

fn limit_scope_is_global(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get("x-ratelimit-scope")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("global"))
        || headers
            .get("x-ratelimit-global")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value == "true" || value == "1")
}

fn body_is_global(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("global").and_then(|flag| flag.as_bool()))
        .unwrap_or(false)
}

fn bucket_reset_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let remaining = headers
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok())?
        .trim()
        .parse::<u64>()
        .ok()?;
    if remaining > 0 {
        return None;
    }
    let header = headers
        .get("x-ratelimit-reset-after")
        .and_then(|value| value.to_str().ok())?;
    let wait = discord_retry_after(Some(header), b"{}");
    Some(wait)
}

fn token_fingerprint(token: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    token.hash(&mut hasher);
    hasher.finish()
}

/// Discord's rate-limit docs: use `Retry-After` or JSON `retry_after` (seconds).
/// Prefer the longer of the two when both are present. Missing, zero, or
/// unparseable values still wait one second. The parsed wait is kept; only
/// adding it to the process clock can saturate, and that stays blocked.
pub(crate) fn discord_retry_after(header: Option<&str>, body: &[u8]) -> Duration {
    let from_header = header.and_then(parse_retry_seconds);
    let from_body = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("retry_after").and_then(|v| v.as_f64()))
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0);
    let seconds = match (from_header, from_body) {
        (Some(header), Some(body)) => header.max(body),
        (Some(header), None) => header,
        (None, Some(body)) => body,
        (None, None) => 1.0,
    };
    let millis = (seconds * 1000.0).ceil().max(1000.0);
    if !millis.is_finite() || millis >= u64::MAX as f64 {
        return Duration::MAX;
    }
    Duration::from_millis(millis as u64)
}

fn parse_retry_seconds(header: &str) -> Option<f64> {
    let seconds: f64 = header.trim().parse().ok()?;
    if seconds.is_finite() && seconds >= 0.0 {
        Some(seconds)
    } else {
        None
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

impl OAuthClient {
    pub fn from_config(cfg: &crate::config::ApiProcessConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("jModGuard (Discord dashboard)")
            .timeout(Duration::from_secs(15))
            .build()
            .context("failed to build OAuth HTTP client")?;
        Ok(Self {
            client_id: cfg.web.oauth_client_id.clone(),
            client_secret: cfg.web.oauth_client_secret.clone(),
            redirect_uri: format!("{}/oauth/callback", cfg.web.base_url.trim_end_matches('/')),
            http,
            guild_list: std::sync::Arc::new(std::sync::Mutex::new(
                super::oauth::GuildListGate::new(),
            )),
            guild_flights: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            retry_db: std::sync::Mutex::new(None),
            #[cfg(test)]
            guilds_endpoint: None,
            #[cfg(test)]
            token_endpoint: None,
            #[cfg(test)]
            me_endpoint: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn discord_retry_after_honors_header_and_json_seconds() {
        assert_eq!(discord_retry_after(Some("2"), b"{}").as_secs(), 2);
        assert_eq!(
            discord_retry_after(Some("2"), br#"{"retry_after":4.5}"#).as_millis(),
            4500
        );
        assert_eq!(discord_retry_after(None, b"{}").as_secs(), 1);
        assert_eq!(discord_retry_after(Some("0"), b"{}").as_secs(), 1);
        assert_eq!(discord_retry_after(Some("120"), b"{}").as_secs(), 120);
        assert_eq!(discord_retry_after(Some("90000"), b"{}").as_secs(), 90000);
        assert!(matches!(
            cooldown_deadline(Instant::now(), Duration::MAX),
            Cooldown::RestOfProcess
        ));
        assert!(matches!(
            cooldown_deadline(Instant::now(), Duration::from_secs(90_000)),
            Cooldown::Until(_)
        ));
        assert_eq!(
            retry_after_header_secs(discord_retry_after(Some("4.2"), b"{}")),
            5
        );
    }

    #[tokio::test]
    async fn slow_guild_lookup_does_not_block_another_token() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let entered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let entered_handler = std::sync::Arc::clone(&entered);
        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/guilds",
                axum::routing::get(|headers: axum::http::HeaderMap| async move {
                    let slow = headers
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        .is_some_and(|value| value.ends_with("slow"));
                    if slow {
                        entered_handler.store(true, std::sync::atomic::Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(400)).await;
                    }
                    axum::Json(serde_json::json!([{
                        "id": "1",
                        "name": "n",
                        "icon": null,
                        "owner": false,
                        "permissions": 32
                    }]))
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        let client = std::sync::Arc::new(OAuthClient {
            client_id: String::new(),
            client_secret: String::new(),
            redirect_uri: String::new(),
            http: reqwest::Client::new(),
            guild_list: std::sync::Arc::new(std::sync::Mutex::new(GuildListGate::new())),
            guild_flights: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            retry_db: std::sync::Mutex::new(None),
            guilds_endpoint: Some(format!("http://{address}/guilds")),
            token_endpoint: None,
            me_endpoint: None,
        });
        let slow_client = std::sync::Arc::clone(&client);
        let slow = tokio::spawn(async move { slow_client.my_guilds(1, "slow").await });
        let wait_started = Instant::now();
        while !entered.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(wait_started.elapsed() < Duration::from_secs(2));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let started = Instant::now();
        client.my_guilds(2, "fast").await.unwrap();
        assert!(started.elapsed() < Duration::from_millis(250));
        slow.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn refreshed_token_for_the_same_user_does_not_bypass_cooldown() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = std::sync::Arc::clone(&calls);
        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/guilds",
                axum::routing::get(move || {
                    let seen = std::sync::Arc::clone(&seen);
                    async move {
                        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        (
                            axum::http::StatusCode::TOO_MANY_REQUESTS,
                            [("retry-after", "30")],
                            axum::Json(serde_json::json!({"retry_after": 30})),
                        )
                    }
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        let client = OAuthClient {
            client_id: String::new(),
            client_secret: String::new(),
            redirect_uri: String::new(),
            http: reqwest::Client::new(),
            guild_list: std::sync::Arc::new(std::sync::Mutex::new(GuildListGate::new())),
            guild_flights: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            retry_db: std::sync::Mutex::new(None),
            guilds_endpoint: Some(format!("http://{address}/guilds")),
            token_endpoint: None,
            me_endpoint: None,
        };
        let first = client.my_guilds(7, "old-token").await.unwrap_err();
        assert!(matches!(first, GuildListError::RateLimited { .. }));
        let second = client.my_guilds(7, "refreshed-token").await.unwrap_err();
        assert!(matches!(
            second,
            GuildListError::RateLimited { fresh: false, .. }
        ));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let other = client.my_guilds(8, "other-user").await.unwrap_err();
        assert!(matches!(other, GuildListError::RateLimited { .. }));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn delayed_rate_limit_body_blocks_the_user_until_it_arrives() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = std::sync::Arc::clone(&calls);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let seen = std::sync::Arc::clone(&seen);
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = socket.read(&mut buf).await;
                    seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    socket
                        .write_all(
                            b"HTTP/1.1 429 Too Many Requests\r\nretry-after: 1\r\ncontent-length: 20\r\nconnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    let _ = socket.write_all(br#"{"retry_after":30}"#).await;
                });
            }
        });
        let client = std::sync::Arc::new(OAuthClient {
            client_id: String::new(),
            client_secret: String::new(),
            redirect_uri: String::new(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            guild_list: std::sync::Arc::new(std::sync::Mutex::new(GuildListGate::new())),
            guild_flights: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            retry_db: std::sync::Mutex::new(None),
            guilds_endpoint: Some(format!("http://{address}/guilds")),
            token_endpoint: None,
            me_endpoint: None,
        });
        let first_client = std::sync::Arc::clone(&client);
        let first = tokio::spawn(async move { first_client.my_guilds(9, "first").await });
        let wait_started = Instant::now();
        while calls.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            assert!(wait_started.elapsed() < Duration::from_secs(2));
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let second = client.my_guilds(9, "second").await.unwrap_err();
        assert!(matches!(second, GuildListError::RateLimited { .. }));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let _ = first.await.unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    fn test_oauth(
        http: reqwest::Client,
        guilds_endpoint: Option<String>,
        token_endpoint: Option<String>,
        me_endpoint: Option<String>,
    ) -> OAuthClient {
        OAuthClient {
            client_id: String::new(),
            client_secret: String::new(),
            redirect_uri: String::new(),
            http,
            guild_list: std::sync::Arc::new(std::sync::Mutex::new(GuildListGate::new())),
            guild_flights: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            retry_db: std::sync::Mutex::new(None),
            guilds_endpoint,
            token_endpoint,
            me_endpoint,
        }
    }

    #[tokio::test]
    async fn concurrent_guild_lookups_share_one_success() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = std::sync::Arc::clone(&calls);
        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/guilds",
                axum::routing::get(move || {
                    let seen = std::sync::Arc::clone(&seen);
                    async move {
                        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        axum::Json(serde_json::json!([{
                            "id": "1",
                            "name": "n",
                            "icon": null,
                            "owner": false,
                            "permissions": 32
                        }]))
                    }
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        let client = std::sync::Arc::new(test_oauth(
            reqwest::Client::new(),
            Some(format!("http://{address}/guilds")),
            None,
            None,
        ));
        let left = std::sync::Arc::clone(&client);
        let right = std::sync::Arc::clone(&client);
        let (first, second) =
            tokio::join!(async move { left.my_guilds(3, "one").await }, async move {
                right.my_guilds(3, "one").await
            },);
        assert_eq!(first.unwrap().len(), 1);
        assert_eq!(second.unwrap().len(), 1);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn identity_rate_limit_is_not_fetched_again() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = std::sync::Arc::clone(&calls);
        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/me",
                axum::routing::get(move || {
                    let seen = std::sync::Arc::clone(&seen);
                    async move {
                        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        (
                            axum::http::StatusCode::TOO_MANY_REQUESTS,
                            [("retry-after", "8")],
                            axum::Json(serde_json::json!({"retry_after": 8})),
                        )
                    }
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        let client = test_oauth(
            reqwest::Client::new(),
            None,
            None,
            Some(format!("http://{address}/me")),
        );
        let first = client.me(4, "token").await.unwrap_err();
        assert!(first.downcast_ref::<DiscordRateLimit>().is_some());
        let second = client.me(4, "token").await.unwrap_err();
        assert!(second.downcast_ref::<DiscordRateLimit>().is_some());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn token_rate_limit_is_not_exchanged_again() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = std::sync::Arc::clone(&calls);
        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/token",
                axum::routing::post(move || {
                    let seen = std::sync::Arc::clone(&seen);
                    async move {
                        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        (
                            axum::http::StatusCode::TOO_MANY_REQUESTS,
                            [("retry-after", "6")],
                            axum::Json(serde_json::json!({"retry_after": 6})),
                        )
                    }
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        let client = test_oauth(
            reqwest::Client::new(),
            None,
            Some(format!("http://{address}/token")),
            None,
        );
        assert!(
            client
                .refresh("one")
                .await
                .unwrap_err()
                .downcast_ref::<DiscordRateLimit>()
                .is_some()
        );
        assert!(
            client
                .refresh("two")
                .await
                .unwrap_err()
                .downcast_ref::<DiscordRateLimit>()
                .is_some()
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn global_guild_limit_blocks_identity_lookup() {
        let guild_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let me_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let guild_seen = std::sync::Arc::clone(&guild_calls);
        let me_seen = std::sync::Arc::clone(&me_calls);
        tokio::spawn(async move {
            let app = axum::Router::new()
                .route(
                    "/guilds",
                    axum::routing::get(move || {
                        let guild_seen = std::sync::Arc::clone(&guild_seen);
                        async move {
                            guild_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            (
                                axum::http::StatusCode::TOO_MANY_REQUESTS,
                                [("retry-after", "20"), ("x-ratelimit-scope", "global")],
                                axum::Json(serde_json::json!({"global": true, "retry_after": 20})),
                            )
                        }
                    }),
                )
                .route(
                    "/me",
                    axum::routing::get(move || {
                        let me_seen = std::sync::Arc::clone(&me_seen);
                        async move {
                            me_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            axum::Json(serde_json::json!({
                                "id": "4",
                                "username": "n",
                                "global_name": null,
                                "avatar": null
                            }))
                        }
                    }),
                );
            axum::serve(listener, app).await.unwrap();
        });
        let client = test_oauth(
            reqwest::Client::new(),
            Some(format!("http://{address}/guilds")),
            None,
            Some(format!("http://{address}/me")),
        );
        let limited = client.my_guilds(4, "token").await.unwrap_err();
        assert!(matches!(
            limited,
            GuildListError::RateLimited { fresh: true, .. }
        ));
        assert!(
            client
                .me(4, "token")
                .await
                .unwrap_err()
                .downcast_ref::<DiscordRateLimit>()
                .is_some()
        );
        assert_eq!(guild_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(me_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
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
        let owner: DiscordGuild = serde_json::from_str(
            r#"{"id":"1","name":"x","icon":null,"owner":true,"permissions":0}"#,
        )
        .unwrap();
        assert!(owner.manage_permission(), "owner bypass");
        assert!(owner.administrator_permission());
        let admin: DiscordGuild = serde_json::from_str(
            r#"{"id":"1","name":"x","icon":null,"owner":false,"permissions":8}"#,
        )
        .unwrap();
        assert!(admin.administrator_permission());
        let manage_only: DiscordGuild = serde_json::from_str(
            r#"{"id":"1","name":"x","icon":null,"owner":false,"permissions":32}"#,
        )
        .unwrap();
        assert!(manage_only.manage_permission());
        assert!(!manage_only.administrator_permission());
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
