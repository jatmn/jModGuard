//! Guild-owned case-removal settings. Not user authorization by themselves.
use super::tenant_logs::{valid_token, verify_scope};
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantSettings {
    pub case_delete_roles: Vec<i64>,
    pub revision: String,
}

fn parse_roles(raw: &str) -> Result<Vec<i64>> {
    let roles: Vec<i64> = serde_json::from_str(raw)?;
    anyhow::ensure!(
        roles.len() <= 32 && roles.iter().all(|id| *id > 0) && {
            let mut unique = roles.clone();
            unique.sort_unstable();
            unique.dedup();
            unique.len() == roles.len()
        },
        TenantDataError::Setting
    );
    Ok(roles)
}

impl GuildDataStore {
    pub async fn tenant_settings(&self, scope: GuildDataScope) -> Result<TenantSettings> {
        self.check_target(&scope.placement)?;
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_primary_read()?;
                let (_, revision) = verify_scope(&tx, &expected, &scope, true)?;
                let row = tx
                    .query_row(
                        "SELECT case_delete_roles FROM guild_tenant_settings WHERE guild_id=?1 AND enrollment_id=?2",
                        params![scope.guild_id(), scope.enrollment.id.as_str()],
                        |r| Ok(r.get::<String>(0)?),
                    )
                    .optional()?;
                tx.finish_read()?;
                Ok(match row {
                    Some(raw) => TenantSettings {
                        case_delete_roles: parse_roles(&raw)?,
                        revision,
                    },
                    None => TenantSettings {
                        case_delete_roles: Vec::new(),
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
        self.check_target(&scope.placement)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        anyhow::ensure!(valid_token(revision), TenantDataError::Revision);
        parse_roles(&serde_json::to_string(&roles)?)?;
        let revision = revision.to_owned();
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let mut tx = conn.transaction_primary_read()?;
                let (_, current) = verify_scope(&tx, &expected, &scope, true)?;
                anyhow::ensure!(revision == current, TenantDataError::Revision);
                let existing = tx
                    .query_row(
                        "SELECT case_delete_roles FROM guild_tenant_settings WHERE guild_id=?1 AND enrollment_id=?2",
                        params![scope.guild_id(), scope.enrollment.id.as_str()],
                        |r| Ok(r.get::<String>(0)?),
                    )
                    .optional()?;
                let encoded = serde_json::to_string(&roles)?;
                if existing.as_deref() == Some(encoded.as_str())
                    || (existing.is_none() && roles.is_empty())
                {
                    tx.finish_read()?;
                    return Ok(current);
                }
                tx.ensure_mutation_receipt()?;
                let next = format!("{:032x}", rand::random::<u128>());
                if existing.is_some() {
                    tx.execute_rows(
                        "UPDATE guild_tenant_settings SET case_delete_roles=?3,revision=?4 WHERE guild_id=?1 AND enrollment_id=?2",
                        params![scope.guild_id(), scope.enrollment.id.as_str(), encoded, next.as_str()], 1,
                    )?;
                } else {
                    tx.execute_rows(
                        "INSERT INTO guild_tenant_settings(guild_id,enrollment_id,case_delete_roles,revision) VALUES(?1,?2,?3,?4)",
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
