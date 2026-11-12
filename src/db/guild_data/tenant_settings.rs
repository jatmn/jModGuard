//! Guild-owned case-removal and Web UI access-role settings. Not user
//! authorization by themselves: callers check live Discord roles separately.
use super::tenant_logs::{valid_token, verify_scope};
use super::*;

/// Web UI access a mapped Discord role grants in one guild. Ordered by reach.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum AccessLevel {
    /// Read-only dashboard, cases, members, logs and configuration.
    Dashboard,
    /// Dashboard plus guild configuration edits.
    Settings,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessRole {
    pub role_id: i64,
    pub level: AccessLevel,
}

/// Highest level any of `roles` grants through `mappings`.
pub fn access_level_for_roles(mappings: &[AccessRole], roles: &[i64]) -> Option<AccessLevel> {
    mappings
        .iter()
        .filter(|mapping| roles.contains(&mapping.role_id))
        .map(|mapping| mapping.level)
        .max()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantSettings {
    pub case_delete_roles: Vec<i64>,
    pub access_roles: Vec<AccessRole>,
    pub revision: String,
}

const MAX_ROLES: usize = 32;

fn parse_roles(raw: &str) -> Result<Vec<i64>> {
    let roles: Vec<i64> = serde_json::from_str(raw)?;
    anyhow::ensure!(
        roles.len() <= MAX_ROLES && roles.iter().all(|id| *id > 0) && {
            let mut unique = roles.clone();
            unique.sort_unstable();
            unique.dedup();
            unique.len() == roles.len()
        },
        TenantDataError::Setting
    );
    Ok(roles)
}

fn parse_access_roles(raw: &str) -> Result<Vec<AccessRole>> {
    let roles: Vec<AccessRole> =
        serde_json::from_str(raw).map_err(|_| anyhow::Error::from(TenantDataError::Setting))?;
    let ids = serde_json::to_string(&roles.iter().map(|r| r.role_id).collect::<Vec<_>>())?;
    parse_roles(&ids)?;
    Ok(roles)
}

#[derive(Clone, Copy)]
enum SettingColumn {
    CaseDeleteRoles,
    AccessRoles,
}

impl SettingColumn {
    fn name(self) -> &'static str {
        match self {
            Self::CaseDeleteRoles => "case_delete_roles",
            Self::AccessRoles => "access_roles",
        }
    }
}

impl GuildDataStore {
    pub async fn tenant_settings(&self, scope: GuildDataScope) -> Result<TenantSettings> {
        self.check_target(&scope.placement)?;
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_primary_snapshot()?;
                let (_, revision) = verify_scope(&tx, &expected, &scope, true)?;
                let row = tx
                    .query_row(
                        "SELECT case_delete_roles,access_roles FROM guild_tenant_settings WHERE guild_id=?1 AND enrollment_id=?2",
                        params![scope.guild_id(), scope.enrollment.id.as_str()],
                        |r| Ok((r.get::<String>(0)?, r.get::<String>(1)?)),
                    )
                    .optional()?;
                tx.finish_read()?;
                Ok(match row {
                    Some((case_delete, access)) => TenantSettings {
                        case_delete_roles: parse_roles(&case_delete)?,
                        access_roles: parse_access_roles(&access)?,
                        revision,
                    },
                    None => TenantSettings {
                        case_delete_roles: Vec::new(),
                        access_roles: Vec::new(),
                        revision,
                    },
                })
            })
            .await
    }

    pub async fn set_case_delete_roles(
        &self,
        scope: GuildDataScope,
        revision: &str,
        roles: Vec<i64>,
    ) -> Result<String> {
        parse_roles(&serde_json::to_string(&roles)?)?;
        let encoded = serde_json::to_string(&roles)?;
        self.set_tenant_setting(scope, revision, SettingColumn::CaseDeleteRoles, encoded)
            .await
    }

    /// Replace this membership's Web UI access-role mappings.
    pub async fn set_access_roles(
        &self,
        scope: GuildDataScope,
        revision: &str,
        roles: Vec<AccessRole>,
    ) -> Result<String> {
        let encoded = serde_json::to_string(&roles)?;
        parse_access_roles(&encoded)?;
        self.set_tenant_setting(scope, revision, SettingColumn::AccessRoles, encoded)
            .await
    }

    async fn set_tenant_setting(
        &self,
        scope: GuildDataScope,
        revision: &str,
        column: SettingColumn,
        encoded: String,
    ) -> Result<String> {
        self.check_target(&scope.placement)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        anyhow::ensure!(valid_token(revision), TenantDataError::Revision);
        let revision = revision.to_owned();
        let expected = self.expected.clone();
        let core = self.core.clone();
        let name = column.name();
        self.db
            .with_conn(move |conn| {
                let mut tx = conn.transaction_primary_write()?;
                if let Err(error) = Self::confirm_captured_enrollment(&core, &scope) {
                    tx.finish_read()?;
                    return Err(error);
                }
                let (_, current) = verify_scope(&tx, &expected, &scope, true)?;
                anyhow::ensure!(revision == current, TenantDataError::Revision);
                let existing = tx
                    .query_row(
                        &format!("SELECT {name} FROM guild_tenant_settings WHERE guild_id=?1 AND enrollment_id=?2"),
                        params![scope.guild_id(), scope.enrollment.id.as_str()],
                        |r| Ok(r.get::<String>(0)?),
                    )
                    .optional()?;
                if existing.as_deref() == Some(encoded.as_str())
                    || (existing.is_none() && encoded == "[]")
                {
                    tx.finish_read()?;
                    return Ok(current);
                }
                tx.ensure_mutation_receipt()?;
                let next = format!("{:032x}", rand::random::<u128>());
                if existing.is_some() {
                    tx.execute_rows(
                        &format!("UPDATE guild_tenant_settings SET {name}=?3,revision=?4 WHERE guild_id=?1 AND enrollment_id=?2"),
                        params![scope.guild_id(), scope.enrollment.id.as_str(), encoded, next.as_str()], 1,
                    )?;
                } else {
                    tx.execute_rows(
                        &format!("INSERT INTO guild_tenant_settings(guild_id,enrollment_id,{name},revision) VALUES(?1,?2,?3,?4)"),
                        params![scope.guild_id(), scope.enrollment.id.as_str(), encoded, next.as_str()], 1,
                    )?;
                }
                tx.execute_rows(
                    "UPDATE guild_memberships SET config_revision=?2 WHERE guild_id=?1",
                    params![scope.guild_id(), next.as_str()], 1,
                )?;
                tx.commit()?;
                Ok(next)
            })
            .await
    }
}
