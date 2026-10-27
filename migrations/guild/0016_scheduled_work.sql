-- Due work for one guild membership. Version 17. No core migration.
CREATE TABLE guild_scheduled_work (
    guild_id INTEGER NOT NULL CHECK(guild_id>0),
    enrollment_id TEXT NOT NULL,
    work_id TEXT NOT NULL CHECK(length(CAST(work_id AS BLOB))=32 AND work_id NOT GLOB '*[^0-9a-f]*'),
    kind TEXT NOT NULL CHECK(kind IN ('softban_unban','ban_expiry','reminder','scheduled_post','user_timezone')),
    subject_id INTEGER NOT NULL CHECK(subject_id>0),
    due_at_ms INTEGER NOT NULL CHECK(due_at_ms>=0),
    payload TEXT NOT NULL CHECK(length(CAST(payload AS BLOB)) BETWEEN 1 AND 2000),
    state TEXT NOT NULL CHECK(state IN ('pending','claimed','done','failed')),
    attempt INTEGER NOT NULL CHECK(attempt>=0 AND attempt<=8),
    claim_until_ms INTEGER CHECK(claim_until_ms IS NULL OR claim_until_ms>0),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms>0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms>=created_at_ms),
    PRIMARY KEY(guild_id, enrollment_id, work_id)
);
CREATE INDEX guild_scheduled_work_due ON guild_scheduled_work(kind, state, due_at_ms);
CREATE UNIQUE INDEX guild_scheduled_work_one_open
    ON guild_scheduled_work(guild_id, enrollment_id, kind, subject_id)
    WHERE state IN ('pending','claimed')
      AND kind IN ('softban_unban','ban_expiry','user_timezone');
CREATE TRIGGER guild_scheduled_work_insert BEFORE INSERT ON guild_scheduled_work
WHEN NOT EXISTS(SELECT 1 FROM guild_memberships WHERE guild_id=NEW.guild_id AND enrollment_id=NEW.enrollment_id AND active=1)
 OR NEW.state!='pending' OR NEW.attempt!=0 OR NEW.claim_until_ms IS NOT NULL
BEGIN SELECT RAISE(ABORT,'scheduled work scope or initial state is invalid'); END;
CREATE TRIGGER guild_scheduled_work_update BEFORE UPDATE ON guild_scheduled_work
WHEN NEW.guild_id!=OLD.guild_id OR NEW.enrollment_id!=OLD.enrollment_id OR NEW.work_id!=OLD.work_id
 OR NEW.kind!=OLD.kind OR NEW.subject_id!=OLD.subject_id OR NEW.created_at_ms!=OLD.created_at_ms
 OR NEW.due_at_ms<OLD.due_at_ms
 OR NEW.attempt<OLD.attempt OR NEW.attempt>OLD.attempt+1
 OR OLD.state IN ('done','failed')
 OR NOT (
      (OLD.state='pending' AND NEW.state='claimed' AND NEW.attempt=OLD.attempt+1 AND NEW.claim_until_ms IS NOT NULL)
      OR (OLD.state='claimed' AND NEW.state='pending' AND NEW.attempt=OLD.attempt AND NEW.claim_until_ms IS NULL)
      OR (OLD.state='claimed' AND NEW.state IN ('done','failed') AND NEW.attempt=OLD.attempt AND NEW.claim_until_ms IS NULL)
      OR (OLD.kind='user_timezone' AND NEW.kind='user_timezone' AND OLD.state='pending' AND NEW.state='pending'
          AND OLD.attempt=0 AND NEW.attempt=0 AND OLD.claim_until_ms IS NULL AND NEW.claim_until_ms IS NULL
          AND NEW.due_at_ms=OLD.due_at_ms AND NEW.payload!=OLD.payload)
    )
BEGIN SELECT RAISE(ABORT,'scheduled work transition refused'); END;
CREATE TRIGGER guild_scheduled_work_delete BEFORE DELETE ON guild_scheduled_work
WHEN NOT (
    EXISTS(SELECT 1 FROM guild_memberships m WHERE m.guild_id=OLD.guild_id AND m.enrollment_id=OLD.enrollment_id AND m.active=0)
    OR OLD.state IN ('done','failed')
)
BEGIN SELECT RAISE(ABORT,'scheduled work delete refused'); END;
DROP TRIGGER guild_membership_delete;
CREATE TRIGGER guild_membership_delete BEFORE DELETE ON guild_memberships
WHEN OLD.active=1
 OR EXISTS(SELECT 1 FROM guild_log_channels WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_cases WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_case_evidence WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_case_history WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_case_sources WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_bunny_evidence WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_tenant_settings WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_seed WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_sync WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_directory WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_stays WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_case_index WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_pending_bans WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_state_observations WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_target_checks WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_audit_recovery WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_unresolved_audits WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_feature WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_scheduled_work WHERE guild_id=OLD.guild_id)
BEGIN SELECT RAISE(ABORT,'guild membership still has live data'); END;
