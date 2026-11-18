-- Selected moderation records only. Deleted cases keep opaque replay keys, not payloads.
CREATE TABLE guild_cases (
    guild_id INTEGER NOT NULL CHECK(guild_id>0),
    enrollment_id TEXT NOT NULL,
    case_id TEXT NOT NULL CHECK(length(CAST(case_id AS BLOB))=32 AND case_id NOT GLOB '*[^0-9a-f]*'),
    source_key TEXT NOT NULL CHECK(length(CAST(source_key AS BLOB)) BETWEEN 1 AND 40),
    payload TEXT,
    deleted INTEGER NOT NULL CHECK(deleted IN (0,1)),
    CHECK((deleted=0 AND payload IS NOT NULL) OR (deleted=1 AND payload IS NULL)),
    PRIMARY KEY(guild_id,enrollment_id,case_id),
    UNIQUE(guild_id,enrollment_id,source_key)
);
CREATE TRIGGER guild_case_insert BEFORE INSERT ON guild_cases
WHEN NOT EXISTS(SELECT 1 FROM guild_memberships WHERE guild_id=NEW.guild_id AND enrollment_id=NEW.enrollment_id AND active=1)
    OR EXISTS(SELECT 1 FROM guild_cases WHERE guild_id=NEW.guild_id AND enrollment_id=NEW.enrollment_id AND (case_id=NEW.case_id OR source_key=NEW.source_key))
BEGIN SELECT RAISE(ABORT,'case scope or identity conflicts'); END;
CREATE TRIGGER guild_case_update BEFORE UPDATE ON guild_cases
WHEN NEW.guild_id!=OLD.guild_id OR NEW.enrollment_id!=OLD.enrollment_id OR NEW.case_id!=OLD.case_id OR NEW.source_key!=OLD.source_key
    OR OLD.deleted!=0 OR NEW.deleted!=1 OR NEW.payload IS NOT NULL
BEGIN SELECT RAISE(ABORT,'case payload is immutable except removal'); END;
CREATE TRIGGER guild_case_delete BEFORE DELETE ON guild_cases
WHEN NOT EXISTS(SELECT 1 FROM guild_memberships WHERE guild_id=OLD.guild_id AND enrollment_id=OLD.enrollment_id AND active=0)
BEGIN SELECT RAISE(ABORT,'case replay keys require closed membership cleanup'); END;
CREATE INDEX guild_cases_visible ON guild_cases(guild_id,enrollment_id,deleted,case_id);
