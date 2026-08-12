//! Shared Discord REST helpers.

use std::future::Future;

use anyhow::{Result, anyhow};
use serenity::model::channel::Channel;
use serenity::model::id::ChannelId;

/// Retry a Discord REST call a few times with short backoff.
///
/// Nickname apply/restore (WebUI and `!setname`) share this so a transient
/// Discord blip does not leave Discord and SQLite permanently diverged.
pub async fn with_retries<T, E, F, Fut>(label: &str, guild_id: u64, mut op: F) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    const MAX_ATTEMPTS: u32 = 3;
    let mut last_err = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if attempt < MAX_ATTEMPTS {
                    tracing::warn!(
                        "{label} attempt {attempt}/{MAX_ATTEMPTS} failed \
                         (guild={guild_id}): {e}"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(150 * u64::from(attempt)))
                        .await;
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.expect("at least one attempt ran"))
}

/// Ensures a configured log destination belongs to the guild whose events it
/// will receive. A channel snowflake alone is not a safe ownership boundary
/// when the bot is installed in multiple guilds.
pub async fn validate_log_channel_guild(
    http: impl AsRef<serenity::http::Http>,
    guild_id: u64,
    channel_id: ChannelId,
) -> Result<()> {
    let channel = http
        .as_ref()
        .get_channel(channel_id)
        .await
        .map_err(|e| anyhow!("failed to resolve log channel {channel_id}: {e}"))?;
    match channel {
        Channel::Guild(channel) if channel.guild_id.get() == guild_id => Ok(()),
        Channel::Guild(channel) => anyhow::bail!(
            "log channel {channel_id} belongs to guild {}, not {guild_id}",
            channel.guild_id
        ),
        Channel::Private(_) => anyhow::bail!("log channel {channel_id} is not a guild channel"),
        _ => anyhow::bail!("log channel {channel_id} is not a guild channel"),
    }
}
