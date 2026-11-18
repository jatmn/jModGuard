-- Coordinated live-data erasure. Closed memberships may be removed only after
-- owned tenant rows are gone. This is not a move and not a departed ledger.
DROP TRIGGER IF EXISTS guild_membership_no_delete;
CREATE TRIGGER guild_membership_delete BEFORE DELETE ON guild_memberships
WHEN OLD.active = 1
    OR EXISTS (SELECT 1 FROM guild_log_channels WHERE guild_id = OLD.guild_id)
    OR EXISTS (SELECT 1 FROM guild_cases WHERE guild_id = OLD.guild_id)
    OR EXISTS (SELECT 1 FROM guild_case_evidence WHERE guild_id = OLD.guild_id)
    OR EXISTS (SELECT 1 FROM guild_case_history WHERE guild_id = OLD.guild_id)
    OR EXISTS (SELECT 1 FROM guild_case_sources WHERE guild_id = OLD.guild_id)
    OR EXISTS (SELECT 1 FROM guild_bunny_evidence WHERE guild_id = OLD.guild_id)
    OR EXISTS (SELECT 1 FROM guild_tenant_settings WHERE guild_id = OLD.guild_id)
BEGIN SELECT RAISE(ABORT, 'guild membership still has live data'); END;

DROP TRIGGER IF EXISTS guild_binding_no_delete;
CREATE TRIGGER guild_binding_delete BEFORE DELETE ON guild_bindings
WHEN EXISTS (SELECT 1 FROM guild_memberships WHERE guild_id = OLD.guild_id)
BEGIN SELECT RAISE(ABORT, 'guild binding still has membership'); END;

CREATE TABLE guild_tenant_settings (
    guild_id INTEGER PRIMARY KEY CHECK(guild_id > 0),
    enrollment_id TEXT NOT NULL
        CHECK(length(CAST(enrollment_id AS BLOB)) = 32 AND enrollment_id NOT GLOB '*[^0-9a-f]*'),
    case_delete_roles TEXT NOT NULL DEFAULT '[]',
    revision TEXT NOT NULL
        CHECK(length(CAST(revision AS BLOB)) = 32 AND revision NOT GLOB '*[^0-9a-f]*')
);
CREATE TRIGGER guild_tenant_settings_insert BEFORE INSERT ON guild_tenant_settings
WHEN NOT EXISTS (
    SELECT 1 FROM guild_memberships
    WHERE guild_id = NEW.guild_id AND enrollment_id = NEW.enrollment_id AND active = 1
)
BEGIN SELECT RAISE(ABORT, 'guild membership is not active'); END;
CREATE TRIGGER guild_tenant_settings_no_replace BEFORE INSERT ON guild_tenant_settings
WHEN EXISTS (SELECT 1 FROM guild_tenant_settings WHERE guild_id = NEW.guild_id)
BEGIN SELECT RAISE(ABORT, 'guild tenant settings already exist'); END;
CREATE TRIGGER guild_tenant_settings_update BEFORE UPDATE ON guild_tenant_settings
WHEN NEW.guild_id != OLD.guild_id
    OR NEW.enrollment_id != OLD.enrollment_id
    OR NOT EXISTS (
        SELECT 1 FROM guild_memberships
        WHERE guild_id = NEW.guild_id AND enrollment_id = NEW.enrollment_id AND active = 1
    )
BEGIN SELECT RAISE(ABORT, 'guild tenant settings scope changed'); END;
