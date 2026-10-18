//! Durable received facts for the gateway ban lifecycle, not Discord commands.
use super::*;
use serde::Deserialize;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayBanPeer {
    pub id: String,
    pub observed_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GatewayBanPayload {
    Ban {
        user_name: String,
        user_display_name: Option<String>,
        owned_by_bot: bool,
        operation_id: Option<String>,
        member_was_banned: bool,
        pending_audits: Vec<i64>,
        overlapping_deliveries: Vec<GatewayBanPeer>,
        audit_after_ms: i64,
    },
    Unban {
        suppressed: bool,
    },
    Audit {
        entry_id: i64,
        attribution: GatewayBanAuditAttribution,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayBanDelivery {
    pub id: String,
    pub enrollment: GuildEnrollment,
    pub user_id: i64,
    pub observed_at: String,
    pub payload: GatewayBanPayload,
    pub case: Option<guild_data::CaseData>,
}

impl GatewayBanDelivery {
    fn kind(&self) -> &'static str {
        match self.payload {
            GatewayBanPayload::Ban { .. } => "ban",
            GatewayBanPayload::Unban { .. } => "unban",
            GatewayBanPayload::Audit { .. } => "audit",
        }
    }
}

/// Save the immutable payload once. In particular a repeated audit callback may
/// have a different cached actor name; its existing delivery remains canonical.
pub async fn save_gateway_ban_delivery(
    db: &Db,
    delivery: GatewayBanDelivery,
) -> Result<GatewayBanDelivery> {
    let observed = DateTime::parse_from_rfc3339(&delivery.observed_at)?.timestamp_millis();
    anyhow::ensure!(
        !delivery.id.is_empty() && delivery.id.len() <= 128 && delivery.user_id > 0,
        "invalid gateway delivery"
    );
    let payload = serde_json::to_string(&delivery)?;
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        super::enrollments::verify_enrollment_conn(&tx, &delivery.enrollment)?;
        super::guild_stores::refuse_fenced_write(&tx, delivery.enrollment.guild_id)?;
        if let Some(saved) = tx.query_row(
            "SELECT payload FROM gateway_ban_deliveries WHERE id=?1",
            params![delivery.id.clone()], |row| Ok(row.get::<String>(0)?),
        ).optional()? {
            let saved: GatewayBanDelivery = serde_json::from_str(&saved)?;
            anyhow::ensure!(saved.enrollment == delivery.enrollment && saved.user_id == delivery.user_id && saved.kind() == delivery.kind(), "gateway delivery identity conflict");
            if !matches!(delivery.payload, GatewayBanPayload::Audit { .. }) {
                anyhow::ensure!(serde_json::to_string(&saved)? == payload, "gateway delivery payload changed");
            }
            tx.finish_read()?;
            return Ok(saved);
        }
        tx.ensure_mutation_receipt()?;
        tx.execute_rows(
            "INSERT INTO gateway_ban_deliveries (id,guild_id,enrollment_id,user_id,kind,observed_at_ms,payload,case_saved)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![delivery.id.clone(),delivery.enrollment.guild_id,delivery.enrollment.id.clone(),delivery.user_id,delivery.kind(),observed,payload,delivery.case.is_none()], 1,
        )?;
        tx.commit()?;
        Ok(delivery)
    }).await
}

/// A bounded round-robin page; an unmatched old audit cannot starve newer work.
pub async fn pending_gateway_ban_deliveries(
    db: &Db,
    after: i64,
) -> Result<Vec<(i64, GatewayBanDelivery, bool, bool)>> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let rows = tx.prepare(
            "SELECT d.rowid,d.payload,d.applied,d.case_saved FROM gateway_ban_deliveries d
             JOIN guild_enrollments e ON e.guild_id=d.guild_id AND e.id=d.enrollment_id AND e.active=1
             WHERE d.rowid>?1 AND (d.applied=0 OR d.case_saved=0) ORDER BY d.rowid LIMIT 32",
        )?.query_map(params![after], |row| Ok((row.get::<i64>(0)?, row.get::<String>(1)?, row.get::<bool>(2)?, row.get::<bool>(3)?)))?
          .collect::<Result<Vec<_>>>()?;
        let rows = rows.into_iter().map(|(id,payload,applied,case_saved)| Ok((id,serde_json::from_str(&payload)?,applied,case_saved))).collect::<Result<_>>()?;
        tx.finish_read()?;
        Ok(rows)
    }).await
}

pub async fn acknowledge_gateway_ban_case(db: &Db, delivery: GatewayBanDelivery) -> Result<()> {
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        super::enrollments::verify_enrollment_conn(&tx, &delivery.enrollment)?;
        super::guild_stores::refuse_fenced_write(&tx, delivery.enrollment.guild_id)?;
        let pending: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM gateway_ban_deliveries WHERE id=?1 AND case_saved=0)",
            params![delivery.id.clone()],
            |row| Ok(row.get(0)?),
        )?;
        if !pending {
            tx.finish_read()?;
            return Ok(());
        }
        tx.ensure_mutation_receipt()?;
        tx.execute_matching(
            "UPDATE gateway_ban_deliveries SET case_saved=1 WHERE id=?1",
            params![delivery.id],
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}

/// Persist a known unban boundary even if there is no old ban row to update.
/// Called by direct successful unbans as well as received gateway unbans.
pub(super) fn record_unban_boundary_conn(
    tx: &crate::db_compat::Transaction,
    enrollment: &GuildEnrollment,
    user: i64,
    at: &str,
) -> Result<()> {
    let observed = DateTime::parse_from_rfc3339(at)?.timestamp_millis();
    let id = format!("unban-boundary:{}:{}:{}", enrollment.id, user, observed);
    let delivery = GatewayBanDelivery {
        id: id.clone(),
        enrollment: enrollment.clone(),
        user_id: user,
        observed_at: at.to_owned(),
        payload: GatewayBanPayload::Unban { suppressed: true },
        case: None,
    };
    tx.execute_rows("INSERT OR IGNORE INTO gateway_ban_deliveries (id,guild_id,enrollment_id,user_id,kind,observed_at_ms,payload,applied,case_saved) VALUES (?1,?2,?3,?4,'unban',?5,?6,1,1)",params![id,enrollment.guild_id,enrollment.id.clone(),user,observed,serde_json::to_string(&delivery)?],1)?;
    Ok(())
}

/// Apply only from the saved payload. Its durable result is committed with the
/// mirror/event write, so retry after a lost response returns the same row.
pub async fn apply_gateway_ban_delivery(db: &Db, id: String) -> Result<Option<GatewayBanWrite>> {
    db.with_conn(move |conn| {
        let tx=conn.transaction_primary_snapshot()?;
        let (payload,applied,result)=tx.query_row("SELECT payload,applied,ban_result FROM gateway_ban_deliveries WHERE id=?1",params![id.clone()],|row|Ok((row.get::<String>(0)?,row.get::<bool>(1)?,row.get::<Option<String>>(2)?))).optional()?.ok_or(EnrollmentChanged)?;
        let delivery:GatewayBanDelivery=serde_json::from_str(&payload)?;
        super::enrollments::verify_enrollment_conn(&tx,&delivery.enrollment)?;
        super::guild_stores::refuse_fenced_write(&tx,delivery.enrollment.guild_id)?;
        tx.finish_read()?;
        if applied { return result.map(|r|serde_json::from_str(&r).map_err(Into::into)).transpose(); }
        match delivery.payload {
            GatewayBanPayload::Ban {user_name,user_display_name,owned_by_bot,operation_id,member_was_banned,pending_audits,audit_after_ms,..} => {
                super::moderation::record_gateway_ban_conn(conn,&delivery.enrollment,delivery.enrollment.guild_id,delivery.user_id,Some(user_name),user_display_name,GATEWAY_BAN_REASON.to_owned(),delivery.observed_at,owned_by_bot,operation_id,member_was_banned,None,pending_audits,audit_after_ms,Some(&id)).map(Some)
            }
            GatewayBanPayload::Unban {suppressed} => {
                let mut tx=conn.transaction_primary_read()?;
                super::enrollments::verify_enrollment_conn(&tx,&delivery.enrollment)?;
                super::guild_stores::refuse_fenced_write(&tx,delivery.enrollment.guild_id)?;
                let applied:bool=tx.query_row("SELECT applied FROM gateway_ban_deliveries WHERE id=?1",params![id.clone()],|row|Ok(row.get(0)?))?;
                if applied {tx.finish_read()?;return Ok(None);}
                tx.ensure_mutation_receipt()?;
                // A delayed unban must not clear a newer ban.
                tx.execute_matching("UPDATE bans SET active=0 WHERE guild_id=?1 AND user_id=?2 AND active=1 AND julianday(created_at)<=julianday(?3)",params![delivery.enrollment.guild_id,delivery.user_id,delivery.observed_at.clone()])?;
                if !suppressed {
                    tx.execute_rows("INSERT INTO events (guild_id,event_type,user_id,data,created_at) VALUES (?1,'member_unban',?2,?3,?4)",params![delivery.enrollment.guild_id,delivery.user_id,serde_json::json!({"source":"discord","gateway_delivery_id":id}).to_string(),delivery.observed_at],1)?;
                }
                tx.execute_matching("UPDATE gateway_ban_deliveries SET applied=1 WHERE id=?1",params![id])?;
                tx.commit()?;
                Ok(None)
            }
            GatewayBanPayload::Audit {..} => anyhow::bail!("audit delivery requires reconciliation"),
        }
    }).await
}

/// Acknowledge only an audit already durably attached by the existing matcher.
pub async fn acknowledge_gateway_ban_audit(db: &Db, delivery: GatewayBanDelivery) -> Result<()> {
    let GatewayBanPayload::Audit { entry_id, .. } = delivery.payload else {
        anyhow::bail!("not an audit delivery")
    };
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        super::enrollments::verify_enrollment_conn(&tx, &delivery.enrollment)?;
        super::guild_stores::refuse_fenced_write(&tx, delivery.enrollment.guild_id)?;
        anyhow::ensure!(
            super::moderation::gateway_audit_entry_id_known_for_user(
                &tx,
                delivery.enrollment.guild_id,
                delivery.user_id,
                entry_id
            )?,
            "audit is not reconciled"
        );
        let applied: bool = tx.query_row(
            "SELECT applied FROM gateway_ban_deliveries WHERE id=?1",
            params![delivery.id.clone()],
            |row| Ok(row.get(0)?),
        )?;
        if applied {
            tx.finish_read()?;
            return Ok(());
        }
        tx.ensure_mutation_receipt()?;
        tx.execute_matching(
            "UPDATE gateway_ban_deliveries SET applied=1 WHERE id=?1",
            params![delivery.id],
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}
