-- Durable object ownership; this schema does not upload or delete Storage objects.
CREATE TABLE guild_case_evidence (
    guild_id INTEGER NOT NULL,
    enrollment_id TEXT NOT NULL,
    case_id TEXT NOT NULL,
    evidence_id TEXT NOT NULL CHECK(length(CAST(evidence_id AS BLOB))=32 AND evidence_id NOT GLOB '*[^0-9a-f]*'),
    storage_id TEXT NOT NULL CHECK(length(CAST(storage_id AS BLOB))=32 AND storage_id NOT GLOB '*[^0-9a-f]*'),
    reserved_bytes INTEGER NOT NULL CHECK(typeof(reserved_bytes)='integer' AND reserved_bytes>0),
    state INTEGER NOT NULL CHECK(state IN (0,1,2,3)),
    cancelled INTEGER NOT NULL CHECK(cancelled IN (0,1)),
    channel_id INTEGER,
    message_id INTEGER,
    attachment_id INTEGER,
    sha256 TEXT,
    PRIMARY KEY(guild_id,enrollment_id,evidence_id),
    CHECK((cancelled=0 AND channel_id>0 AND message_id>0 AND attachment_id>0 AND channel_id IS NOT NULL AND message_id IS NOT NULL AND attachment_id IS NOT NULL)
        OR (cancelled=1 AND channel_id IS NULL AND message_id IS NULL AND attachment_id IS NULL)),
    CHECK((state=0 AND sha256 IS NULL) OR
        (state=1 AND cancelled=0 AND sha256 IS NOT NULL AND length(CAST(sha256 AS BLOB))=64 AND sha256 NOT GLOB '*[^0-9a-f]*') OR
        (state IN (2,3) AND cancelled=1 AND sha256 IS NULL))
);
CREATE INDEX guild_evidence_parent ON guild_case_evidence(guild_id,enrollment_id,case_id);
CREATE INDEX guild_evidence_work ON guild_case_evidence(guild_id,enrollment_id,state,evidence_id);
CREATE INDEX guild_evidence_cancel ON guild_case_evidence(guild_id,enrollment_id,cancelled,evidence_id);
CREATE TRIGGER guild_evidence_insert BEFORE INSERT ON guild_case_evidence
WHEN NEW.state!=0 OR NEW.cancelled!=0
    OR NOT EXISTS(SELECT 1 FROM guild_cases c JOIN guild_memberships m USING(guild_id,enrollment_id) WHERE c.guild_id=NEW.guild_id AND c.enrollment_id=NEW.enrollment_id AND c.case_id=NEW.case_id AND c.deleted=0 AND m.active=1)
    OR EXISTS(SELECT 1 FROM guild_case_evidence WHERE guild_id=NEW.guild_id AND enrollment_id=NEW.enrollment_id AND evidence_id=NEW.evidence_id)
BEGIN SELECT RAISE(ABORT,'evidence parent or identity conflicts'); END;
CREATE TRIGGER guild_evidence_update BEFORE UPDATE ON guild_case_evidence
WHEN NEW.guild_id!=OLD.guild_id OR NEW.enrollment_id!=OLD.enrollment_id OR NEW.case_id!=OLD.case_id OR NEW.evidence_id!=OLD.evidence_id OR NEW.storage_id!=OLD.storage_id OR NEW.reserved_bytes!=OLD.reserved_bytes
    OR NOT (
        (OLD.state=0 AND NEW.state=1 AND OLD.cancelled=0 AND NEW.channel_id IS OLD.channel_id AND NEW.message_id IS OLD.message_id AND NEW.attachment_id IS OLD.attachment_id
            AND EXISTS(SELECT 1 FROM guild_cases c JOIN guild_memberships m USING(guild_id,enrollment_id) WHERE c.guild_id=OLD.guild_id AND c.enrollment_id=OLD.enrollment_id AND c.case_id=OLD.case_id AND c.deleted=0 AND m.active=1))
        OR (OLD.state=0 AND NEW.state=2)
        OR (OLD.state=2 AND NEW.state=3)
        OR (OLD.cancelled=0 AND NEW.cancelled=1 AND ((OLD.state=0 AND NEW.state=0) OR (OLD.state=1 AND NEW.state=2))
            AND EXISTS(SELECT 1 FROM guild_cases c JOIN guild_memberships m USING(guild_id,enrollment_id) WHERE c.guild_id=OLD.guild_id AND c.enrollment_id=OLD.enrollment_id AND c.case_id=OLD.case_id AND (c.deleted=1 OR m.active=0)))
    )
BEGIN SELECT RAISE(ABORT,'invalid evidence transition'); END;
CREATE TRIGGER guild_evidence_delete BEFORE DELETE ON guild_case_evidence
WHEN OLD.state!=3 OR NOT EXISTS(SELECT 1 FROM guild_memberships WHERE guild_id=OLD.guild_id AND enrollment_id=OLD.enrollment_id AND active=0)
BEGIN SELECT RAISE(ABORT,'evidence cleanup must settle before closed purge'); END;
CREATE TRIGGER guild_case_cancel_evidence AFTER UPDATE OF deleted ON guild_cases WHEN NEW.deleted=1
BEGIN UPDATE guild_case_evidence SET cancelled=1,state=CASE state WHEN 0 THEN 0 ELSE 2 END,channel_id=NULL,message_id=NULL,attachment_id=NULL,sha256=NULL
    WHERE guild_id=NEW.guild_id AND enrollment_id=NEW.enrollment_id AND case_id=NEW.case_id AND cancelled=0; END;
CREATE TRIGGER guild_case_purge_evidence BEFORE DELETE ON guild_cases
BEGIN
    SELECT RAISE(ABORT,'case has unsettled evidence') WHERE EXISTS(SELECT 1 FROM guild_case_evidence WHERE guild_id=OLD.guild_id AND enrollment_id=OLD.enrollment_id AND case_id=OLD.case_id AND state!=3);
    DELETE FROM guild_case_evidence WHERE guild_id=OLD.guild_id AND enrollment_id=OLD.enrollment_id AND case_id=OLD.case_id;
END;
