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

/// Gateway observations for onboarding setup. They are display-only and may
/// lag Discord. Unknown bot permissions are distinct from a known denial.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OnboardingTrapSetup {
    pub bot_can_kick: Option<bool>,
    pub roles_at_or_above_bot: Vec<String>,
    pub roles_with_grants: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles_with_member_grants: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ServerNames {
    pub roles: Vec<ServerRoleName>,
    pub channels: Vec<ServerChannelName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onboarding_trap: Option<OnboardingTrapSetup>,
    /// When the bot last changed this snapshot; None before its first one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_ms: Option<i64>,
}

fn read_names_row(
    tx: &impl SqlExecutor,
    scope: &GuildDataScope,
) -> Result<Option<(String, Option<String>, i64)>> {
    tx.query_row(
        "SELECT names, onboarding_trap, observed_at_ms FROM guild_server_names
         WHERE guild_id=?1 AND enrollment_id=?2",
        params![scope.guild_id(), scope.enrollment.id.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .optional()
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
        if let Some(setup) = self.onboarding_trap.as_mut() {
            let retained: std::collections::HashSet<_> =
                self.roles.iter().map(|role| role.id.as_str()).collect();
            for ids in [
                &mut setup.roles_at_or_above_bot,
                &mut setup.roles_with_grants,
                &mut setup.roles_with_member_grants,
            ] {
                ids.retain(|id| retained.contains(id.as_str()));
                ids.sort_unstable();
                ids.dedup();
            }
        }
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
        let mut names = names.normalized()?;
        let setup = names
            .onboarding_trap
            .take()
            .map(|setup| serde_json::to_string(&setup))
            .transpose()?;
        let encoded = serde_json::to_string(&names)?;
        anyhow::ensure!(encoded.len() <= NAMES_MAX_BYTES, TenantDataError::Setting);
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let mut tx = conn.transaction_primary_write()?;
                verify_scope(&tx, &expected, &scope, true)?;
                let existing = read_names_row(&tx, &scope)?;
                if existing
                    .as_ref()
                    .is_some_and(|(stored, stored_setup, observed)| {
                        *observed > observed_at_ms || (stored == &encoded && stored_setup == &setup)
                    })
                {
                    tx.finish_read()?;
                    return Ok(false);
                }
                tx.ensure_mutation_receipt()?;
                tx.execute_rows(
                    "INSERT INTO guild_server_names(guild_id,enrollment_id,names,observed_at_ms)
                     VALUES(?1,?2,?3,?4)
                     ON CONFLICT(guild_id) DO UPDATE SET names=excluded.names,
                       observed_at_ms=excluded.observed_at_ms
                     WHERE guild_server_names.enrollment_id=excluded.enrollment_id
                       AND guild_server_names.observed_at_ms<=excluded.observed_at_ms",
                    params![
                        scope.guild_id(),
                        scope.enrollment.id.as_str(),
                        encoded,
                        observed_at_ms,
                    ],
                    1,
                )?;
                if let Some(setup) = setup {
                    tx.execute_rows(
                        "UPDATE guild_server_names SET onboarding_trap=?3
                         WHERE guild_id=?1 AND enrollment_id=?2 AND observed_at_ms=?4",
                        params![
                            scope.guild_id(),
                            scope.enrollment.id.as_str(),
                            setup,
                            observed_at_ms
                        ],
                        1,
                    )?;
                }
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
                let row = read_names_row(&tx, &scope)?;
                tx.finish_read()?;
                Ok(match row {
                    Some((names, setup, observed_at_ms)) => {
                        let mut names: ServerNames = serde_json::from_str(&names)?;
                        names.observed_at_ms = Some(observed_at_ms);
                        if let Some(setup) = setup {
                            names.onboarding_trap = Some(serde_json::from_str(&setup)?);
                        }
                        names
                    }
                    None => ServerNames::default(),
                })
            })
            .await
    }
}
