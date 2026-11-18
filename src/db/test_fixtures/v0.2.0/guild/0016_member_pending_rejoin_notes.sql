-- Durable rejoin-note receipt for a stay. Survives process restarts and member-sync
-- that commits the stay before the gateway member-add handler runs.
CREATE TABLE guild_member_pending_rejoin_notes (
    guild_id INTEGER NOT NULL CHECK(guild_id > 0),
    enrollment_id TEXT NOT NULL,
    user_id INTEGER NOT NULL CHECK(user_id > 0),
    stay_id TEXT NOT NULL,
    joined_at_ms INTEGER,
    PRIMARY KEY(guild_id, enrollment_id, user_id),
    FOREIGN KEY(guild_id, enrollment_id, stay_id) REFERENCES guild_member_stays(guild_id, enrollment_id, stay_id)
);
CREATE INDEX guild_member_pending_rejoin_stay
    ON guild_member_pending_rejoin_notes(guild_id, enrollment_id, stay_id);

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
 OR EXISTS(SELECT 1 FROM guild_member_pending_rejoin_notes WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_state_observations WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_target_checks WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_audit_recovery WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_unresolved_audits WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_feature WHERE guild_id=OLD.guild_id)
BEGIN SELECT RAISE(ABORT,'guild membership still has live data'); END;
