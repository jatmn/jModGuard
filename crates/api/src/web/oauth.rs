//! Discord OAuth client and dashboard identity models.
use anyhow::{Context as _, Result};
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub struct OAuthClient {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    pub http: reqwest::Client,
    /// Serializes `/users/@me/guilds` so concurrent callers share one cooldown.
    pub(crate) guild_list: tokio::sync::Mutex<GuildListGate>,
    #[cfg(test)]
    pub guilds_endpoint: Option<String>,
    #[cfg(test)]
    pub token_endpoint: Option<String>,
}

pub(crate) struct GuildListGate {
    /// Access token → earliest instant another guild-list request may start.
    cooldowns: HashMap<String, Instant>,
}

impl GuildListGate {
    pub(crate) fn new() -> Self {
        Self {
            cooldowns: HashMap::new(),
        }
    }

    fn remaining(&mut self, token: &str) -> Option<Duration> {
        let now = Instant::now();
        self.cooldowns.retain(|_, until| *until > now);
        self.cooldowns
            .get(token)
            .map(|until| until.saturating_duration_since(now))
    }

    fn block_until(&mut self, token: &str, retry_after: Duration) {
        let until = Instant::now() + retry_after;
        self.cooldowns.insert(token.to_owned(), until);
    }
}

/// Upstream guild-list failure. Only `Unauthorized` justifies an OAuth refresh.
#[derive(Debug)]
pub enum GuildListError {
    RateLimited { retry_after: Duration },
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
            "Discord is temporarily rate limiting guild checks. Try again in about {seconds}s."
        )
    }
}

impl std::error::Error for DiscordRateLimit {}

/// Seconds advertised to the browser. Missing or zero Discord values still
/// impose a one-second floor so callers do not hammer immediately. Values
/// above a minute are capped so a bad header cannot stall the dashboard.
pub(crate) fn retry_after_header_secs(retry_after: Duration) -> u64 {
    let millis = retry_after.as_millis();
    let seconds = millis.div_ceil(1000) as u64;
    seconds.clamp(1, 60)
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

    pub async fn my_guilds(
        &self,
        access_token: &str,
    ) -> std::result::Result<Vec<DiscordGuild>, GuildListError> {
        // One shared gate: a 429 from any page blocks later callers, including
        // concurrent ones, until Discord's advertised cooldown elapses. The
        // lock is not held while the caller waits out that cooldown.
        let mut gate = self.guild_list.lock().await;
        if let Some(retry_after) = gate.remaining(access_token) {
            return Err(GuildListError::RateLimited { retry_after });
        }
        // Discord paginates at 200 guilds; a single GET silently drops every
        // server past the first page and deny WebUI manage access for them.
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
            if status.as_u16() == 401 {
                return Err(GuildListError::Unauthorized);
            }
            if status.as_u16() == 429 {
                let header = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let bytes = resp.bytes().await.unwrap_or_default();
                let retry_after = discord_retry_after(header.as_deref(), &bytes);
                gate.block_until(access_token, retry_after);
                return Err(GuildListError::RateLimited { retry_after });
            }
            if !status.is_success() {
                return Err(GuildListError::Failed(anyhow::anyhow!(
                    "discord guilds request failed with {status}"
                )));
            }
            let page: Vec<DiscordGuild> = match resp.json().await {
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
}

/// Discord's rate-limit docs: use `Retry-After` or JSON `retry_after` (seconds).
/// Prefer the longer of the two when both are present. Missing, zero, or
/// unparseable values still wait one second. The shared gate caps at 60s.
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
    let millis = (seconds * 1000.0).ceil().clamp(1000.0, 60_000.0) as u64;
    Duration::from_millis(millis)
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
        assert_eq!(discord_retry_after(Some("120"), b"{}").as_secs(), 60);
        assert_eq!(
            retry_after_header_secs(discord_retry_after(Some("4.2"), b"{}")),
            5
        );
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
