//! Shared Discord REST helpers.

use std::future::Future;

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
