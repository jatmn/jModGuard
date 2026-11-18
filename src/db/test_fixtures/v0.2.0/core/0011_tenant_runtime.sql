-- Application-owned tenant lifecycle. Not a departed-guild membership ledger.
-- The backup_cleanup_intents schema is historical and retained for compatibility;
-- current services neither produce nor consume these rows.
CREATE TABLE guild_erasure_jobs (
    guild_id INTEGER PRIMARY KEY CHECK(guild_id > 0),
    enrollment_id TEXT NOT NULL
        CHECK(length(enrollment_id) = 32 AND enrollment_id NOT GLOB '*[^0-9a-f]*'),
    joined_at_ns INTEGER NOT NULL CHECK(typeof(joined_at_ns) = 'integer' AND joined_at_ns > 0),
    store_id TEXT NOT NULL
        CHECK(length(store_id) = 32 AND store_id NOT GLOB '*[^0-9a-f]*'),
    placement_revision TEXT NOT NULL
        CHECK(length(placement_revision) = 32 AND placement_revision NOT GLOB '*[^0-9a-f]*'),
    created_at INTEGER NOT NULL
);
CREATE TRIGGER guild_erasure_jobs_no_replace BEFORE INSERT ON guild_erasure_jobs
WHEN EXISTS (SELECT 1 FROM guild_erasure_jobs WHERE guild_id = NEW.guild_id)
BEGIN SELECT RAISE(ABORT, 'guild erasure already queued'); END;

CREATE TABLE backup_cleanup_intents (
    intent_id TEXT PRIMARY KEY
        CHECK(length(intent_id) = 32 AND intent_id NOT GLOB '*[^0-9a-f]*'),
    kind TEXT NOT NULL CHECK(kind IN ('guild', 'case', 'evidence')),
    guild_id INTEGER NOT NULL CHECK(guild_id > 0),
    enrollment_id TEXT CHECK(
        enrollment_id IS NULL
        OR (length(enrollment_id) = 32 AND enrollment_id NOT GLOB '*[^0-9a-f]*')
    ),
    case_id TEXT CHECK(
        case_id IS NULL
        OR (length(case_id) = 32 AND case_id NOT GLOB '*[^0-9a-f]*')
    ),
    evidence_id TEXT CHECK(
        evidence_id IS NULL
        OR (length(evidence_id) = 32 AND evidence_id NOT GLOB '*[^0-9a-f]*')
    ),
    object_key TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX backup_cleanup_intents_guild ON backup_cleanup_intents(guild_id, created_at, intent_id);
