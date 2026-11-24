-- Optional schema 23 extension. The bot installs it in a scoped write.
-- Existing column lists, JSON contracts and the schema stamp stay compatible.
ALTER TABLE guild_server_names ADD COLUMN onboarding_trap TEXT
    CHECK(onboarding_trap IS NULL OR
          (json_valid(onboarding_trap) AND length(CAST(onboarding_trap AS BLOB)) <= 32768));

ALTER TABLE guild_cases ADD COLUMN hide_from_member_history INTEGER NOT NULL DEFAULT 0
    CHECK(hide_from_member_history IN (0, 1));

CREATE TRIGGER guild_case_member_visibility_immutable
BEFORE UPDATE OF hide_from_member_history ON guild_cases
WHEN NEW.hide_from_member_history != OLD.hide_from_member_history
BEGIN SELECT RAISE(ABORT, 'case member visibility is immutable'); END;

-- A previous bot can still run its original index backfill after rollback.
-- Ignore attempts to index private cases, including pre-release JSON rows.
CREATE TRIGGER guild_member_case_index_hide_private
AFTER INSERT ON guild_member_case_index
WHEN EXISTS (
    SELECT 1 FROM guild_cases c
    WHERE c.guild_id=NEW.guild_id AND c.enrollment_id=NEW.enrollment_id
      AND c.case_id=NEW.case_id AND c.deleted=0
      AND (c.hide_from_member_history=1 OR
           CASE WHEN json_valid(c.payload) THEN json_extract(c.payload,'$.hide_from_member_history')=1 END)
)
BEGIN
    DELETE FROM guild_member_case_index WHERE guild_id=NEW.guild_id
      AND enrollment_id=NEW.enrollment_id AND case_id=NEW.case_id;
END;

-- An older writer refreshes the names with its original column list. Drop
-- the previous observation. A new writer sets it after updating the names.
CREATE TRIGGER guild_server_names_clear_onboarding_observation
AFTER UPDATE OF names, observed_at_ms ON guild_server_names
WHEN OLD.onboarding_trap IS NOT NULL
BEGIN
    UPDATE guild_server_names SET onboarding_trap=NULL
    WHERE guild_id=NEW.guild_id AND enrollment_id=NEW.enrollment_id;
END;
