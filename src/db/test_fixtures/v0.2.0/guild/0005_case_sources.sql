-- One application operation and one Discord audit entry can identify the same case.
-- Retain both opaque keys after payload removal, including a late explicit association.
CREATE TABLE guild_case_sources (
    guild_id INTEGER NOT NULL,
    enrollment_id TEXT NOT NULL,
    case_id TEXT NOT NULL,
    source_key TEXT NOT NULL CHECK(length(CAST(source_key AS BLOB)) BETWEEN 1 AND 40),
    source_kind INTEGER NOT NULL CHECK(source_kind IN (0,1)),
    CHECK((source_kind=0 AND length(CAST(source_key AS BLOB))=36 AND substr(source_key,1,4)='app:' AND substr(source_key,5) NOT GLOB '*[^0-9a-f]*')
        OR (source_kind=1 AND substr(source_key,1,8)='discord:' AND CAST(substr(source_key,9) AS INTEGER)>0 AND substr(source_key,9)=CAST(CAST(substr(source_key,9) AS INTEGER) AS TEXT))),
    PRIMARY KEY(guild_id,enrollment_id,source_key),
    UNIQUE(guild_id,enrollment_id,case_id,source_kind)
);
INSERT INTO guild_case_sources
SELECT guild_id,enrollment_id,case_id,source_key,
    CASE WHEN source_key LIKE 'app:%' THEN 0 WHEN source_key LIKE 'discord:%' THEN 1 ELSE -1 END
FROM guild_cases;
CREATE TRIGGER guild_case_source_insert BEFORE INSERT ON guild_case_sources
WHEN NOT EXISTS(SELECT 1 FROM guild_cases c JOIN guild_memberships m USING(guild_id,enrollment_id) WHERE c.guild_id=NEW.guild_id AND c.enrollment_id=NEW.enrollment_id AND c.case_id=NEW.case_id AND m.active=1)
    OR EXISTS(SELECT 1 FROM guild_case_sources WHERE guild_id=NEW.guild_id AND enrollment_id=NEW.enrollment_id AND (source_key=NEW.source_key OR (case_id=NEW.case_id AND source_kind=NEW.source_kind)))
BEGIN SELECT RAISE(ABORT,'case source scope or identity conflicts'); END;
CREATE TRIGGER guild_case_source_update BEFORE UPDATE ON guild_case_sources
BEGIN SELECT RAISE(ABORT,'case sources are immutable'); END;
CREATE TRIGGER guild_case_source_delete BEFORE DELETE ON guild_case_sources
WHEN NOT EXISTS(SELECT 1 FROM guild_memberships WHERE guild_id=OLD.guild_id AND enrollment_id=OLD.enrollment_id AND active=0)
BEGIN SELECT RAISE(ABORT,'case sources require closed membership cleanup'); END;
CREATE TRIGGER guild_case_original_source AFTER INSERT ON guild_cases
BEGIN
    INSERT INTO guild_case_sources VALUES(NEW.guild_id,NEW.enrollment_id,NEW.case_id,NEW.source_key,
        CASE WHEN NEW.source_key LIKE 'app:%' THEN 0 WHEN NEW.source_key LIKE 'discord:%' THEN 1 ELSE -1 END);
END;
CREATE TRIGGER guild_case_purge_sources BEFORE DELETE ON guild_cases
BEGIN DELETE FROM guild_case_sources WHERE guild_id=OLD.guild_id AND enrollment_id=OLD.enrollment_id AND case_id=OLD.case_id; END;
