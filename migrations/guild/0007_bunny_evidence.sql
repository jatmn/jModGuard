-- A PUT attempt is single-use. Only a recorded provider acceptance permits settlement.
CREATE TABLE guild_bunny_evidence (
    guild_id INTEGER NOT NULL,
    enrollment_id TEXT NOT NULL,
    evidence_id TEXT NOT NULL,
    storage_id TEXT NOT NULL,
    binding TEXT NOT NULL CHECK(length(CAST(binding AS BLOB)) BETWEEN 1 AND 256),
    sha256 TEXT CHECK(sha256 IS NULL OR (length(CAST(sha256 AS BLOB))=64 AND sha256 NOT GLOB '*[^0-9a-f]*')),
    accepted INTEGER NOT NULL CHECK(accepted IN (0,1)),
    PRIMARY KEY(guild_id,enrollment_id,evidence_id)
);
CREATE INDEX guild_bunny_resource ON guild_bunny_evidence(storage_id);
CREATE TRIGGER guild_bunny_insert BEFORE INSERT ON guild_bunny_evidence
WHEN NEW.accepted!=0 OR NEW.sha256 IS NULL
 OR NOT EXISTS(SELECT 1 FROM guild_case_evidence e JOIN guild_memberships m USING(guild_id,enrollment_id) WHERE e.guild_id=NEW.guild_id AND e.enrollment_id=NEW.enrollment_id AND e.evidence_id=NEW.evidence_id AND e.storage_id=NEW.storage_id AND e.state=0 AND e.cancelled=0 AND m.active=1)
 OR EXISTS(SELECT 1 FROM guild_bunny_evidence WHERE guild_id=NEW.guild_id AND enrollment_id=NEW.enrollment_id AND evidence_id=NEW.evidence_id)
 OR EXISTS(SELECT 1 FROM guild_bunny_evidence WHERE storage_id=NEW.storage_id AND binding!=NEW.binding)
BEGIN SELECT RAISE(ABORT,'Bunny evidence identity conflicts'); END;
CREATE TRIGGER guild_bunny_update BEFORE UPDATE ON guild_bunny_evidence
WHEN NEW.guild_id!=OLD.guild_id OR NEW.enrollment_id!=OLD.enrollment_id OR NEW.evidence_id!=OLD.evidence_id OR NEW.storage_id!=OLD.storage_id OR NEW.binding!=OLD.binding OR NOT (
    (NEW.sha256 IS OLD.sha256 AND OLD.accepted=0 AND NEW.accepted=1)
    OR (OLD.sha256 IS NOT NULL AND NEW.sha256 IS NULL AND OLD.accepted=NEW.accepted
        AND EXISTS(SELECT 1 FROM guild_case_evidence WHERE guild_id=OLD.guild_id AND enrollment_id=OLD.enrollment_id AND evidence_id=OLD.evidence_id AND cancelled=1))
 )
BEGIN SELECT RAISE(ABORT,'Bunny evidence attempt is immutable'); END;
CREATE TRIGGER guild_bunny_delete BEFORE DELETE ON guild_bunny_evidence
WHEN EXISTS(SELECT 1 FROM guild_case_evidence WHERE guild_id=OLD.guild_id AND enrollment_id=OLD.enrollment_id AND evidence_id=OLD.evidence_id)
BEGIN SELECT RAISE(ABORT,'Bunny evidence remains owned'); END;
CREATE TRIGGER guild_bunny_settlement BEFORE UPDATE OF state ON guild_case_evidence
WHEN NEW.state!=0 AND EXISTS(SELECT 1 FROM guild_bunny_evidence WHERE guild_id=OLD.guild_id AND enrollment_id=OLD.enrollment_id AND evidence_id=OLD.evidence_id AND (accepted=0 OR (NEW.state=1 AND sha256 IS NOT NEW.sha256)))
BEGIN SELECT RAISE(ABORT,'Bunny PUT outcome remains unresolved'); END;
CREATE TRIGGER guild_bunny_purge AFTER DELETE ON guild_case_evidence
BEGIN DELETE FROM guild_bunny_evidence WHERE guild_id=OLD.guild_id AND enrollment_id=OLD.enrollment_id AND evidence_id=OLD.evidence_id; END;
CREATE TRIGGER guild_bunny_forget AFTER UPDATE OF cancelled ON guild_case_evidence WHEN NEW.cancelled=1
BEGIN UPDATE guild_bunny_evidence SET sha256=NULL WHERE guild_id=NEW.guild_id AND enrollment_id=NEW.enrollment_id AND evidence_id=NEW.evidence_id AND sha256 IS NOT NULL; END;
