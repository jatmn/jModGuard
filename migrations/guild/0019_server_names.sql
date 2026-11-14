-- Compatible upgrade from guild schema 19 to 20. The bot's latest view of this
-- membership's role and channel names, for Web UI pickers. Display data only:
-- never used for permissions or moderation decisions.
CREATE TABLE guild_server_names (
    guild_id INTEGER PRIMARY KEY CHECK(guild_id > 0),
    enrollment_id TEXT NOT NULL
        CHECK(length(CAST(enrollment_id AS BLOB)) = 32 AND enrollment_id NOT GLOB '*[^0-9a-f]*'),
    names TEXT NOT NULL CHECK(json_valid(names) AND length(CAST(names AS BLOB)) <= 524288),
    observed_at_ms INTEGER NOT NULL CHECK(observed_at_ms > 0),
    FOREIGN KEY(guild_id) REFERENCES guild_bindings(guild_id)
);
CREATE TRIGGER guild_server_names_insert BEFORE INSERT ON guild_server_names
WHEN NOT EXISTS (
    SELECT 1 FROM guild_memberships
    WHERE guild_id = NEW.guild_id AND enrollment_id = NEW.enrollment_id AND active = 1
)
BEGIN SELECT RAISE(ABORT,'server names scope is not active'); END;
CREATE TRIGGER guild_server_names_update BEFORE UPDATE ON guild_server_names
WHEN NEW.guild_id != OLD.guild_id OR NEW.enrollment_id != OLD.enrollment_id
BEGIN SELECT RAISE(ABORT,'server names scope is immutable'); END;

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
 OR EXISTS(SELECT 1 FROM guild_server_names WHERE guild_id=OLD.guild_id)
BEGIN SELECT RAISE(ABORT,'guild membership still has live data'); END;
