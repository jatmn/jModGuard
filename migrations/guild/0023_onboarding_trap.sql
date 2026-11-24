-- Schema 24 requires the paired application upgrade. Keep display observations
-- and immutable visibility outside the shared snapshot and case JSON.
ALTER TABLE guild_server_names ADD COLUMN onboarding_trap TEXT
    CHECK(onboarding_trap IS NULL OR
          (json_valid(onboarding_trap) AND length(CAST(onboarding_trap AS BLOB)) <= 32768));

ALTER TABLE guild_cases ADD COLUMN hide_from_member_history INTEGER NOT NULL DEFAULT 0
    CHECK(hide_from_member_history IN (0, 1));

CREATE TRIGGER guild_case_member_visibility_immutable
BEFORE UPDATE OF hide_from_member_history ON guild_cases
WHEN NEW.hide_from_member_history != OLD.hide_from_member_history
BEGIN SELECT RAISE(ABORT, 'case member visibility is immutable'); END;
