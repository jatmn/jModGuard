-- Compatible upgrade from guild schema 10 (0010) to 11. 0010 creates the member
-- directory; this step adds gap_generation. Existing in-flight reconciliation
-- has no captured coverage generation, so it cannot clear a gap.
ALTER TABLE guild_member_seed ADD COLUMN gap_generation INTEGER NOT NULL DEFAULT 0;
UPDATE guild_member_seed SET gap_generation=1 WHERE reconciliation_needed=1;

ALTER TABLE guild_member_sync ADD COLUMN last_success_processed INTEGER;
ALTER TABLE guild_member_sync ADD COLUMN last_success_inserted INTEGER;
ALTER TABLE guild_member_sync ADD COLUMN last_success_updated INTEGER;
ALTER TABLE guild_member_sync ADD COLUMN last_success_unchanged INTEGER;
ALTER TABLE guild_member_sync ADD COLUMN reconcile_gap_generation INTEGER;
UPDATE guild_member_sync
SET last_success_processed=processed,
    last_success_inserted=inserted,
    last_success_updated=updated,
    last_success_unchanged=unchanged
WHERE status='complete' AND last_success_at_ms IS NOT NULL;

DROP TRIGGER guild_membership_delete;
CREATE TABLE guild_member_state_observations_v10 (
    guild_id INTEGER NOT NULL,
    enrollment_id TEXT NOT NULL,
    observation_id TEXT NOT NULL,
    user_id INTEGER NOT NULL CHECK(user_id>0),
    kind TEXT NOT NULL CHECK(kind IN ('member_absent','ban_absent','ban_present','membership_changed')),
    observed_at_ms INTEGER NOT NULL,
    PRIMARY KEY(guild_id,enrollment_id,observation_id)
);
INSERT INTO guild_member_state_observations_v10
SELECT guild_id,enrollment_id,observation_id,user_id,kind,observed_at_ms
FROM guild_member_state_observations;
DROP TABLE guild_member_state_observations;
ALTER TABLE guild_member_state_observations_v10 RENAME TO guild_member_state_observations;
CREATE INDEX guild_member_state_observations_user
ON guild_member_state_observations(guild_id,enrollment_id,user_id,observed_at_ms,observation_id);

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
 OR EXISTS(SELECT 1 FROM guild_member_feature WHERE guild_id=OLD.guild_id)
BEGIN SELECT RAISE(ABORT,'guild membership still has live data'); END;
