-- An own-executor Discord audit entry may race its application's exact source
-- link. Preserve the dated audit fact without creating a second canonical case.
CREATE TABLE guild_member_unresolved_audits (
    guild_id INTEGER NOT NULL CHECK(guild_id>0),
    enrollment_id TEXT NOT NULL,
    entry_id INTEGER NOT NULL CHECK(entry_id>0),
    user_id INTEGER NOT NULL CHECK(user_id>0),
    action TEXT NOT NULL CHECK(action IN ('ban','unban')),
    occurred_at_ms INTEGER NOT NULL CHECK(occurred_at_ms>0),
    executor_id INTEGER CHECK(executor_id IS NULL OR executor_id>0),
    reason TEXT CHECK(reason IS NULL OR length(CAST(reason AS BLOB))<=4096),
    PRIMARY KEY(guild_id,enrollment_id,entry_id)
);
CREATE INDEX guild_member_unresolved_audits_user
ON guild_member_unresolved_audits(guild_id,enrollment_id,user_id,occurred_at_ms,entry_id);
CREATE TRIGGER guild_member_unresolved_audit_insert BEFORE INSERT ON guild_member_unresolved_audits
WHEN NOT EXISTS(SELECT 1 FROM guild_memberships m WHERE m.guild_id=NEW.guild_id AND m.enrollment_id=NEW.enrollment_id AND m.active=1)
 OR EXISTS(SELECT 1 FROM guild_case_sources s WHERE s.guild_id=NEW.guild_id AND s.enrollment_id=NEW.enrollment_id AND s.source_key=('discord:' || NEW.entry_id))
BEGIN SELECT RAISE(ABORT,'unresolved audit scope or source conflicts'); END;
CREATE TRIGGER guild_member_unresolved_audit_update BEFORE UPDATE ON guild_member_unresolved_audits
BEGIN SELECT RAISE(ABORT,'unresolved audit facts are immutable'); END;
CREATE TRIGGER guild_member_unresolved_audit_delete BEFORE DELETE ON guild_member_unresolved_audits
WHEN NOT EXISTS(SELECT 1 FROM guild_case_sources s WHERE s.guild_id=OLD.guild_id AND s.enrollment_id=OLD.enrollment_id AND s.source_key=('discord:' || OLD.entry_id))
 AND NOT EXISTS(SELECT 1 FROM guild_memberships m WHERE m.guild_id=OLD.guild_id AND m.enrollment_id=OLD.enrollment_id AND m.active=0)
BEGIN SELECT RAISE(ABORT,'unresolved audit requires exact source or closed-scope erasure'); END;

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
BEGIN SELECT RAISE(ABORT,'guild membership still has live data'); END;
