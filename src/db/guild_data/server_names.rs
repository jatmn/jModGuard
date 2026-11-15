//! The bot's latest view of a membership's role and channel names (#621), so
//! Web UI pickers can show names instead of raw IDs. Display data only: never
//! used for permissions or moderation decisions. One bounded JSON row per
//! membership keeps a whole snapshot to a single write.
use super::tenant_logs::verify_scope;
use super::*;

/// Discord allows 250 roles and 500 channels; extra entries are dropped.
pub(super) const SERVER_NAME_ROLE_LIMIT: usize = 300;
pub(super) const SERVER_NAME_CHANNEL_LIMIT: usize = 600;
const NAME_CHARS: usize = 100;
/// Matches the table CHECK; the limits above stay well under it even when
/// every name needs JSON escapes.
const NAMES_MAX_BYTES: usize = 1_048_576;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerRoleName {
    /// Decimal snowflake, as the Web UI wire requires.
    pub id: String,
    pub name: String,
    /// Discord role color as 0xRRGGBB; 0 means no color.
    pub color: u32,
    pub position: i64,
    /// Managed by an integration or bot; members cannot be given it by hand.
    pub managed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerChannelName {
    /// Decimal snowflake, as the Web UI wire requires.
    pub id: String,
    pub name: String,
    /// text, voice, category, announcement, stage, forum, media or other.
    pub kind: String,
    pub parent_id: Option<String>,
    pub position: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerNames {
    pub roles: Vec<ServerRoleName>,
    pub channels: Vec<ServerChannelName>,
    /// When the bot last changed this snapshot; None before its first one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_ms: Option<i64>,
}

fn snowflake(id: &str) -> bool {
    !id.is_empty() && id.len() <= 20 && id.parse::<u64>().is_ok_and(|value| value > 0)
}

fn short(name: &str) -> String {
    name.chars().take(NAME_CHARS).collect()
}

impl ServerNames {
    /// Sort, deduplicate, bound and validate a snapshot before it is stored.
    fn normalized(mut self) -> Result<Self> {
        self.observed_at_ms = None;
        anyhow::ensure!(
            self.roles.iter().all(|role| snowflake(&role.id))
                && self.channels.iter().all(|channel| {
                    snowflake(&channel.id) && channel.parent_id.as_deref().is_none_or(snowflake)
                }),
            TenantDataError::Setting
        );
        for role in &mut self.roles {
            role.name = short(&role.name);
            role.color &= 0x00ff_ffff;
        }
        for channel in &mut self.channels {
            channel.name = short(&channel.name);
            if !matches!(
                channel.kind.as_str(),
                "text" | "voice" | "category" | "announcement" | "stage" | "forum" | "media"
            ) {
                channel.kind = "other".into();
            }
        }
        // Order is total, so a reordered or duplicated event stores the same text.
        self.roles.sort_by(|a, b| {
            b.position
                .cmp(&a.position)
                .then_with(|| a.id.len().cmp(&b.id.len()).then_with(|| a.id.cmp(&b.id)))
                .then_with(|| a.name.cmp(&b.name))
        });
        self.roles.dedup_by(|a, b| a.id == b.id);
        self.roles.truncate(SERVER_NAME_ROLE_LIMIT);
        self.channels.sort_by(|a, b| {
            a.position
                .cmp(&b.position)
                .then_with(|| a.id.len().cmp(&b.id.len()).then_with(|| a.id.cmp(&b.id)))
                .then_with(|| a.name.cmp(&b.name))
        });
        self.channels.dedup_by(|a, b| a.id == b.id);
        self.channels.truncate(SERVER_NAME_CHANNEL_LIMIT);
        Ok(self)
    }
}

impl GuildDataStore {
    /// Replace this membership's role and channel names. An unchanged snapshot
    /// writes nothing. Returns whether a new snapshot was stored.
    pub async fn replace_server_names(
        &self,
        scope: GuildDataScope,
        names: ServerNames,
        observed_at_ms: i64,
    ) -> Result<bool> {
        self.check_target(&scope.placement)?;
        anyhow::ensure!(observed_at_ms > 0, TenantDataError::Setting);
        let encoded = serde_json::to_string(&names.normalized()?)?;
        anyhow::ensure!(encoded.len() <= NAMES_MAX_BYTES, TenantDataError::Setting);
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let mut tx = conn.transaction_primary_write()?;
                verify_scope(&tx, &expected, &scope, true)?;
                let existing = tx
                    .query_row(
                        "SELECT names FROM guild_server_names WHERE guild_id=?1 AND enrollment_id=?2",
                        params![scope.guild_id(), scope.enrollment.id.as_str()],
                        |r| Ok(r.get::<String>(0)?),
                    )
                    .optional()?;
                if existing.as_deref() == Some(encoded.as_str()) {
                    tx.finish_read()?;
                    return Ok(false);
                }
                tx.ensure_mutation_receipt()?;
                tx.execute_rows(
                    "INSERT INTO guild_server_names(guild_id,enrollment_id,names,observed_at_ms) VALUES(?1,?2,?3,?4)
                     ON CONFLICT(guild_id) DO UPDATE SET names=excluded.names,observed_at_ms=excluded.observed_at_ms",
                    params![
                        scope.guild_id(),
                        scope.enrollment.id.as_str(),
                        encoded,
                        observed_at_ms
                    ],
                    1,
                )?;
                tx.commit()?;
                Ok(true)
            })
            .await
    }

    /// This membership's latest role and channel names; empty before the bot
    /// stores its first snapshot.
    pub async fn server_names(&self, scope: GuildDataScope) -> Result<ServerNames> {
        self.check_target(&scope.placement)?;
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_primary_snapshot()?;
                verify_scope(&tx, &expected, &scope, true)?;
                let row = tx
                    .query_row(
                        "SELECT names,observed_at_ms FROM guild_server_names WHERE guild_id=?1 AND enrollment_id=?2",
                        params![scope.guild_id(), scope.enrollment.id.as_str()],
                        |r| Ok((r.get::<String>(0)?, r.get::<i64>(1)?)),
                    )
                    .optional()?;
                tx.finish_read()?;
                Ok(match row {
                    Some((names, observed_at_ms)) => ServerNames {
                        observed_at_ms: Some(observed_at_ms),
                        ..serde_json::from_str(&names)?
                    },
                    None => ServerNames::default(),
                })
            })
            .await
    }
}
