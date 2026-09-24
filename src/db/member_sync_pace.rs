//! One durable request slot shared by the member importer across guild stores.

use super::*;

#[derive(Debug, Clone)]
pub struct MemberSyncRequestClaim {
    pub bot_id: i64,
    pub worker_id: String,
    pub generation: i64,
}

/// Reserve a request before calling Discord. An unknown commit outcome leaves
/// the conservative deadline and lease in place until a later claim can prove
/// that the slot is free; callers must not retry the HTTP request immediately.
pub async fn claim_member_sync_request(
    db: &Db,
    bot_id: i64,
    worker_id: String,
    now_ms: i64,
    min_interval_ms: i64,
    lease_ms: i64,
) -> Result<Option<MemberSyncRequestClaim>> {
    anyhow::ensure!(
        bot_id > 0
            && now_ms > 0
            && min_interval_ms >= 15_000
            && (30_000..=300_000).contains(&lease_ms)
            && !worker_id.is_empty()
            && worker_id.len() <= 64,
        "invalid member sync request claim"
    );
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        let row = tx.query_row(
            "SELECT request_after_ms,lease_owner,lease_until_ms,generation FROM member_sync_pacing WHERE bot_id=?1",
            params![bot_id],
            |r| Ok((r.get::<i64>(0)?, r.get::<Option<String>>(1)?, r.get::<Option<i64>>(2)?, r.get::<i64>(3)?)),
        ).optional()?;
        let (after, owner, until, generation) = row.unwrap_or((0, None, None, 0));
        if after > now_ms || owner.is_some() && until.is_some_and(|v| v > now_ms) {
            tx.finish_read()?;
            return Ok(None);
        }
        tx.ensure_mutation_receipt()?;
        let next_generation = generation.checked_add(1).context("member sync pacing generation exhausted")?;
        tx.execute(
            "INSERT INTO member_sync_pacing(bot_id,request_after_ms,lease_owner,lease_until_ms,generation)
             VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(bot_id) DO UPDATE SET
               request_after_ms=MAX(member_sync_pacing.request_after_ms,excluded.request_after_ms),
               lease_owner=excluded.lease_owner,lease_until_ms=excluded.lease_until_ms,generation=excluded.generation",
            params![bot_id, now_ms.saturating_add(min_interval_ms), worker_id.as_str(), now_ms.saturating_add(lease_ms), next_generation],
        )?;
        tx.commit()?;
        Ok(Some(MemberSyncRequestClaim { bot_id, worker_id, generation: next_generation }))
    }).await
}

/// A Serenity rate-limit callback may discover a later provider deadline while
/// the request is still sleeping. Persist it immediately, independent of the
/// HTTP result, so another worker cannot consume an earlier slot.
pub async fn extend_member_sync_request_after(
    db: &Db,
    bot_id: i64,
    deadline_ms: i64,
) -> Result<()> {
    anyhow::ensure!(
        bot_id > 0 && deadline_ms > 0,
        "invalid member sync pacing deadline"
    );
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        tx.ensure_mutation_receipt()?;
        tx.execute(
            "INSERT INTO member_sync_pacing(bot_id,request_after_ms) VALUES(?1,?2)
             ON CONFLICT(bot_id) DO UPDATE SET request_after_ms=MAX(member_sync_pacing.request_after_ms,excluded.request_after_ms)",
            params![bot_id, deadline_ms],
        )?;
        tx.commit()
    }).await
}

pub async fn finish_member_sync_request(
    db: &Db,
    claim: MemberSyncRequestClaim,
    request_after_ms: i64,
) -> Result<bool> {
    anyhow::ensure!(request_after_ms > 0, "invalid member sync pacing deadline");
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        tx.ensure_mutation_receipt()?;
        let changed = tx.execute(
            "UPDATE member_sync_pacing SET request_after_ms=MAX(request_after_ms,?4),lease_owner=NULL,lease_until_ms=NULL
             WHERE bot_id=?1 AND lease_owner=?2 AND generation=?3",
            params![claim.bot_id, claim.worker_id, claim.generation, request_after_ms],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacing_survives_reopen_and_provider_extension() {
        let path = std::env::temp_dir().join(format!(
            "member_sync_pacing_{:032x}.db",
            rand::random::<u128>()
        ));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let db = Db::open(&path, 2).unwrap();
        runtime.block_on(async {
            let first =
                claim_member_sync_request(&db, 10, "worker-a".into(), 100_000, 15_000, 90_000)
                    .await
                    .unwrap()
                    .unwrap();
            assert!(
                claim_member_sync_request(&db, 10, "worker-b".into(), 100_001, 15_000, 90_000)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(
                claim_member_sync_request(&db, 11, "worker-b".into(), 100_001, 15_000, 90_000)
                    .await
                    .unwrap()
                    .is_some()
            );
            extend_member_sync_request_after(&db, 10, 200_000)
                .await
                .unwrap();
            assert!(
                finish_member_sync_request(&db, first, 115_000)
                    .await
                    .unwrap()
            );
        });
        drop(db);
        let reopened = Db::open(&path, 2).unwrap();
        runtime.block_on(async {
            assert!(
                claim_member_sync_request(
                    &reopened,
                    10,
                    "worker-c".into(),
                    199_999,
                    15_000,
                    90_000
                )
                .await
                .unwrap()
                .is_none()
            );
            assert!(
                claim_member_sync_request(
                    &reopened,
                    10,
                    "worker-c".into(),
                    200_000,
                    15_000,
                    90_000
                )
                .await
                .unwrap()
                .is_some()
            );
        });
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}
