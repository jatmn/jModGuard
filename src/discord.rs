//! Shared Discord REST helpers.

use anyhow::{Result, anyhow};
use serenity::model::channel::Channel;
use serenity::model::id::ChannelId;

/// Bounds every reconciliation REST attempt so one half-open Discord request
/// cannot stall durable work for every guild indefinitely.
pub const DISCORD_REST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NicknameFailureClass {
    Transient,
    Dormant,
    Terminal,
}

/// Classify a Discord nickname failure into the durable outbox lifecycle.
/// Transport failures, rate limits, and server failures may recover. A guild
/// the bot cannot currently see is dormant until `guild_create` reactivates
/// it. Other client rejections require operator action and are terminal.
pub fn classify_nickname_failure(error: &serenity::Error) -> (NicknameFailureClass, String) {
    if let serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response)) = error {
        return classify_nickname_response(response.status_code.as_u16(), response.error.code);
    }
    if matches!(
        error,
        serenity::Error::Http(
            serenity::http::HttpError::Request(_)
                | serenity::http::HttpError::RateLimitI64F64
                | serenity::http::HttpError::RateLimitUtf8
        )
    ) {
        return (NicknameFailureClass::Transient, "transport".into());
    }
    (NicknameFailureClass::Terminal, "client_error".into())
}

fn classify_nickname_response(status: u16, discord_code: isize) -> (NicknameFailureClass, String) {
    let code = format!("discord_{discord_code}");
    if matches!(discord_code, 10004 | 50001) {
        return (NicknameFailureClass::Dormant, code);
    }
    if status == 429 || status >= 500 {
        return (NicknameFailureClass::Transient, code);
    }
    (NicknameFailureClass::Terminal, code)
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
        Channel::Guild(channel)
            if channel.guild_id.get() == guild_id && channel.is_text_based() =>
        {
            Ok(())
        }
        Channel::Guild(channel) if channel.guild_id.get() == guild_id => {
            anyhow::bail!("log channel {channel_id} cannot receive messages")
        }
        Channel::Guild(channel) => anyhow::bail!(
            "log channel {channel_id} belongs to guild {}, not {guild_id}",
            channel.guild_id
        ),
        Channel::Private(_) => anyhow::bail!("log channel {channel_id} is not a guild channel"),
        _ => anyhow::bail!("log channel {channel_id} is not a guild channel"),
    }
}

#[cfg(test)]
mod tests {
    use super::{NicknameFailureClass, classify_nickname_response};

    #[test]
    fn nickname_failure_lifecycle_classification() {
        assert_eq!(
            classify_nickname_response(404, 10004).0,
            NicknameFailureClass::Dormant
        );
        assert_eq!(
            classify_nickname_response(403, 50001).0,
            NicknameFailureClass::Dormant
        );
        assert_eq!(
            classify_nickname_response(403, 50013).0,
            NicknameFailureClass::Terminal
        );
        assert_eq!(
            classify_nickname_response(400, 50035).0,
            NicknameFailureClass::Terminal
        );
        assert_eq!(
            classify_nickname_response(429, 0).0,
            NicknameFailureClass::Transient
        );
        assert_eq!(
            classify_nickname_response(503, 0).0,
            NicknameFailureClass::Transient
        );
        assert_eq!(
            super::classify_nickname_failure(&serenity::Error::Http(
                serenity::http::HttpError::RateLimitUtf8,
            ))
            .0,
            NicknameFailureClass::Transient
        );
    }
}
