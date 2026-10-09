-- Guild-local write fence. Core fences live in another database, so a status
-- read must see this row inside its own transaction.
CREATE TABLE guild_local_write_fence (
    guild_id INTEGER PRIMARY KEY CHECK(guild_id > 0),
    fenced_at_ms INTEGER NOT NULL
);

-- Durable writer generation for this store. A newer holder raises it before
-- acting. A leased commit must see its own generation in the same transaction.
CREATE TABLE guild_writer_epoch (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    generation INTEGER NOT NULL CHECK(generation > 0)
);

-- Case payloads stay immutable except removal, plus one Attempted -> final
-- outcome change that keeps every other field.
DROP TRIGGER guild_case_update;
CREATE TRIGGER guild_case_update BEFORE UPDATE ON guild_cases
WHEN NEW.guild_id!=OLD.guild_id OR NEW.enrollment_id!=OLD.enrollment_id OR NEW.case_id!=OLD.case_id OR NEW.source_key!=OLD.source_key
 OR OLD.deleted!=0
 OR NOT (
      (NEW.deleted=1 AND NEW.payload IS NULL)
      OR (
           NEW.deleted=0 AND OLD.payload IS NOT NULL AND NEW.payload IS NOT NULL
           AND json_valid(OLD.payload) AND json_valid(NEW.payload)
           AND json_extract(OLD.payload,'$.outcome')='attempted'
           AND json_extract(NEW.payload,'$.outcome') IN ('succeeded','failed','uncertain')
           AND json_set(OLD.payload,'$.outcome', json_extract(NEW.payload,'$.outcome')) = NEW.payload
      )
 )
BEGIN SELECT RAISE(ABORT,'case payload is immutable except removal'); END;

-- A ban inserted as Attempted does not trip the insert-time retain trigger.
-- Settling that same row to succeeded records the same ban fact.
CREATE TRIGGER guild_case_settle_retain AFTER UPDATE OF payload ON guild_cases
WHEN NEW.deleted=0 AND json_valid(NEW.payload) AND json_valid(OLD.payload)
 AND json_extract(OLD.payload,'$.outcome')='attempted'
 AND json_extract(NEW.payload,'$.outcome')='succeeded'
 AND json_extract(NEW.payload,'$.action')='ban'
BEGIN
 INSERT INTO guild_member_directory(guild_id,enrollment_id,user_id,ever_banned,observed_at_ms)
 VALUES(NEW.guild_id,NEW.enrollment_id,CAST(json_extract(NEW.payload,'$.subject_id') AS INTEGER),1,CAST(json_extract(NEW.payload,'$.occurred_at_ms') AS INTEGER))
 ON CONFLICT(guild_id,enrollment_id,user_id) DO UPDATE SET ever_banned=1,cleanup_after_ms=NULL,revision=revision+1;
END;
