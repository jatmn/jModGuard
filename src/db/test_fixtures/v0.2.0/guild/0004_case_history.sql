-- Append-only additions. Their lifetime and access belong to the parent case.
CREATE TABLE guild_case_history (
    guild_id INTEGER NOT NULL,
    enrollment_id TEXT NOT NULL,
    case_id TEXT NOT NULL,
    entry_id TEXT NOT NULL CHECK(length(CAST(entry_id AS BLOB))=32 AND entry_id NOT GLOB '*[^0-9a-f]*'),
    sequence INTEGER NOT NULL CHECK(typeof(sequence)='integer' AND sequence>0),
    payload TEXT NOT NULL,
    PRIMARY KEY(guild_id,enrollment_id,case_id,entry_id),
    UNIQUE(guild_id,enrollment_id,case_id,sequence)
);
CREATE TRIGGER guild_case_history_insert BEFORE INSERT ON guild_case_history
WHEN NOT EXISTS(SELECT 1 FROM guild_cases c JOIN guild_memberships m ON m.guild_id=c.guild_id AND m.enrollment_id=c.enrollment_id
    WHERE c.guild_id=NEW.guild_id AND c.enrollment_id=NEW.enrollment_id AND c.case_id=NEW.case_id AND c.deleted=0 AND m.active=1)
    OR EXISTS(SELECT 1 FROM guild_case_history WHERE guild_id=NEW.guild_id AND enrollment_id=NEW.enrollment_id AND case_id=NEW.case_id
        AND (entry_id=NEW.entry_id OR sequence=NEW.sequence))
BEGIN SELECT RAISE(ABORT,'case history parent or identity conflicts'); END;
CREATE TRIGGER guild_case_history_update BEFORE UPDATE ON guild_case_history
BEGIN SELECT RAISE(ABORT,'case history is immutable'); END;
CREATE TRIGGER guild_case_history_delete BEFORE DELETE ON guild_case_history
WHEN NOT EXISTS(SELECT 1 FROM guild_cases WHERE guild_id=OLD.guild_id AND enrollment_id=OLD.enrollment_id AND case_id=OLD.case_id AND deleted=1)
    AND NOT EXISTS(SELECT 1 FROM guild_memberships WHERE guild_id=OLD.guild_id AND enrollment_id=OLD.enrollment_id AND active=0)
BEGIN SELECT RAISE(ABORT,'case history requires parent removal or closure'); END;
CREATE TRIGGER guild_case_remove_history AFTER UPDATE OF deleted ON guild_cases WHEN NEW.deleted=1
BEGIN DELETE FROM guild_case_history WHERE guild_id=NEW.guild_id AND enrollment_id=NEW.enrollment_id AND case_id=NEW.case_id; END;
CREATE TRIGGER guild_case_purge_history BEFORE DELETE ON guild_cases
BEGIN DELETE FROM guild_case_history WHERE guild_id=OLD.guild_id AND enrollment_id=OLD.enrollment_id AND case_id=OLD.case_id; END;
