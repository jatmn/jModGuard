-- Fresh core schema at v0.2.0 (user_version 23).
-- The initializer stamps user_version in the same transaction.

CREATE TABLE guild_settings (
    guild_id INTEGER NOT NULL PRIMARY KEY,
    settings TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE rules (
    guild_id INTEGER NOT NULL,
    rule_id  TEXT NOT NULL,
    data     TEXT NOT NULL,
    PRIMARY KEY (guild_id, rule_id)
);

CREATE TABLE commands (
    guild_id INTEGER NOT NULL,
    name     TEXT NOT NULL,
    data     TEXT NOT NULL,
    PRIMARY KEY (guild_id, name)
);

CREATE TABLE templates (
    guild_id   INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    template   TEXT NOT NULL,
    PRIMARY KEY (guild_id, event_type)
);

CREATE TABLE log_channels (
    guild_id   INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    channel_id INTEGER NOT NULL,
    PRIMARY KEY (guild_id, event_type)
);

CREATE TABLE badwords (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id INTEGER NOT NULL,
    pattern TEXT NOT NULL,
    note    TEXT
);

CREATE INDEX idx_badwords_guild ON badwords(guild_id);

CREATE TABLE events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id   INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    user_id    INTEGER,
    actor_id   INTEGER,
    data       TEXT,
    created_at TEXT NOT NULL
);

CREATE INDEX idx_events_guild ON events(guild_id, created_at);

CREATE INDEX idx_events_user ON events(user_id);

CREATE TABLE warns (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id     INTEGER NOT NULL,
    user_id      INTEGER NOT NULL,
    moderator_id INTEGER NOT NULL,
    reason       TEXT,
    created_at   TEXT NOT NULL
);

CREATE INDEX idx_warns_user ON warns(guild_id, user_id);

CREATE TABLE bans (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id     INTEGER NOT NULL,
    user_id      INTEGER NOT NULL,
    moderator_id INTEGER,
    reason       TEXT,
    created_at   TEXT NOT NULL,
    active       INTEGER NOT NULL DEFAULT 1
, user_name TEXT, user_display_name TEXT, moderator_name TEXT, operation_id TEXT, expires_at INTEGER, expiry_enrollment_id TEXT);

CREATE INDEX idx_bans_guild ON bans(guild_id, user_id);

CREATE TABLE meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE legacy_global_log_channels (
    event_type TEXT PRIMARY KEY,
    channel_id INTEGER NOT NULL
);

CREATE TABLE nickname_reconcile (
    guild_id  INTEGER PRIMARY KEY,
    generation INTEGER NOT NULL
, state TEXT NOT NULL DEFAULT 'pending', attempt_count INTEGER NOT NULL DEFAULT 0, next_attempt_at INTEGER NOT NULL DEFAULT 0, last_error_code TEXT, last_error TEXT, last_error_at INTEGER, job_id TEXT, enrollment_id TEXT, nickname TEXT);

CREATE TABLE mutation_receipts (
    receipt_id TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_mutation_receipts_created_at ON mutation_receipts (created_at);

CREATE INDEX idx_nickname_reconcile_due
    ON nickname_reconcile (state, next_attempt_at, guild_id);

CREATE TABLE bot_actions (
    id TEXT PRIMARY KEY,
    guild_id INTEGER NOT NULL CHECK(guild_id > 0),
    actor_id INTEGER NOT NULL CHECK(actor_id > 0),
    request_key TEXT NOT NULL,
    request_created_at INTEGER NOT NULL,
    resource TEXT NOT NULL,
    payload TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('pending','claimed','running','succeeded','failed','uncertain','cancelled','expired')),
    outcome TEXT,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    claim TEXT,
    claimed_at INTEGER,
    finished_at INTEGER, enrollment_id TEXT, joined_at_ns INTEGER,
    UNIQUE(guild_id, request_key)
);

CREATE INDEX bot_actions_pending ON bot_actions(state,created_at,id);

CREATE INDEX bot_actions_guild_recent ON bot_actions(guild_id,created_at DESC,id);

CREATE INDEX bot_actions_finished ON bot_actions(max(finished_at,request_created_at),id)
    WHERE state IN ('succeeded','failed','cancelled','expired') AND request_created_at>0;

CREATE TABLE guild_enrollments (
    guild_id INTEGER PRIMARY KEY CHECK(guild_id > 0),
    id TEXT NOT NULL UNIQUE,
    joined_at_ns INTEGER NOT NULL CHECK(joined_at_ns > 0),
    active INTEGER NOT NULL CHECK(active IN (0,1))
);

CREATE UNIQUE INDEX bot_actions_active_resource ON bot_actions(guild_id,enrollment_id,resource)
    WHERE state IN ('pending','claimed','running','uncertain');

CREATE INDEX bot_actions_enrollment ON bot_actions(guild_id,enrollment_id,created_at DESC,id DESC);

CREATE INDEX bot_actions_guild_state ON bot_actions(guild_id,state);

CREATE TABLE guild_store_catalog (
    store_id TEXT PRIMARY KEY NOT NULL
        CHECK(length(store_id) = 32 AND store_id NOT GLOB '*[^0-9a-f]*'),
    dedicated_guild_id INTEGER
        CHECK(dedicated_guild_id IS NULL OR (typeof(dedicated_guild_id) = 'integer' AND dedicated_guild_id > 0))
);

CREATE TABLE guild_store_placements (
    guild_id INTEGER PRIMARY KEY CHECK(guild_id > 0),
    store_id TEXT NOT NULL,
    revision TEXT NOT NULL
        CHECK(length(revision) = 32 AND revision NOT GLOB '*[^0-9a-f]*')
);

CREATE INDEX guild_store_placements_store ON guild_store_placements(store_id);

CREATE TRIGGER guild_store_placement_insert BEFORE INSERT ON guild_store_placements
WHEN NOT EXISTS (
    SELECT 1 FROM guild_store_catalog WHERE store_id=NEW.store_id
    AND (dedicated_guild_id IS NULL OR dedicated_guild_id=NEW.guild_id)
)
BEGIN SELECT RAISE(ABORT, 'invalid guild store placement'); END;

CREATE TRIGGER guild_store_placement_update BEFORE UPDATE ON guild_store_placements
WHEN NOT EXISTS (
    SELECT 1 FROM guild_store_catalog WHERE store_id=NEW.store_id
    AND (dedicated_guild_id IS NULL OR dedicated_guild_id=NEW.guild_id)
)
BEGIN SELECT RAISE(ABORT, 'invalid guild store placement'); END;

CREATE TRIGGER guild_store_catalog_no_replace BEFORE INSERT ON guild_store_catalog
WHEN EXISTS (SELECT 1 FROM guild_store_catalog WHERE store_id=NEW.store_id)
BEGIN SELECT RAISE(ABORT, 'guild store identity is already registered'); END;

CREATE TRIGGER guild_store_catalog_immutable BEFORE UPDATE ON guild_store_catalog
BEGIN SELECT RAISE(ABORT, 'guild store identity and ownership are immutable'); END;

CREATE TRIGGER guild_store_catalog_referenced BEFORE DELETE ON guild_store_catalog
WHEN EXISTS (SELECT 1 FROM guild_store_placements WHERE store_id=OLD.store_id)
BEGIN SELECT RAISE(ABORT, 'guild store is still referenced'); END;

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

CREATE TABLE guild_write_fences (
    guild_id INTEGER PRIMARY KEY CHECK(guild_id > 0),
    generation TEXT NOT NULL
        CHECK(length(generation) = 32 AND generation NOT GLOB '*[^0-9a-f]*'),
    phase TEXT NOT NULL CHECK(phase IN ('freeze', 'copy', 'verify', 'switch')),
    destination_store_id TEXT NOT NULL
        CHECK(length(destination_store_id) = 32 AND destination_store_id NOT GLOB '*[^0-9a-f]*'),
    destination_revision TEXT NOT NULL
        CHECK(length(destination_revision) = 32 AND destination_revision NOT GLOB '*[^0-9a-f]*'),
    created_at INTEGER NOT NULL
);

CREATE TRIGGER guild_write_fence_no_replace BEFORE INSERT ON guild_write_fences
WHEN EXISTS (SELECT 1 FROM guild_write_fences WHERE guild_id = NEW.guild_id)
BEGIN SELECT RAISE(ABORT, 'guild move already fenced'); END;

CREATE TRIGGER guild_write_fence_destination BEFORE INSERT ON guild_write_fences
WHEN NOT EXISTS (
    SELECT 1 FROM guild_store_catalog WHERE store_id = NEW.destination_store_id
    AND (dedicated_guild_id IS NULL OR dedicated_guild_id = NEW.guild_id)
)
BEGIN SELECT RAISE(ABORT, 'invalid guild move destination'); END;

CREATE TABLE log_filters (
    guild_id INTEGER NOT NULL CHECK(guild_id > 0),
    event_type TEXT NOT NULL,
    data TEXT NOT NULL,
    PRIMARY KEY (guild_id, event_type)
);

CREATE TABLE component_updates (
    component TEXT PRIMARY KEY CHECK (component IN ('bot', 'api', 'webui')),
    channel TEXT NOT NULL DEFAULT 'latest' CHECK (channel IN ('latest', 'stable', 'nightly')),
    cadence TEXT NOT NULL DEFAULT 'off' CHECK (cadence IN ('off', 'daily', 'weekly', 'monthly')),
    candidate TEXT,
    checked_at INTEGER,
    check_error TEXT,
    desired TEXT,
    desired_launcher TEXT,
    installed TEXT,
    installed_launcher TEXT,
    launcher_sha256 TEXT,
    launcher_installed_at INTEGER,
    previous TEXT,
    observed TEXT,
    observed_at INTEGER,
    installed_at INTEGER,
    state TEXT NOT NULL DEFAULT 'idle',
    last_error TEXT,
    lease_owner TEXT,
    lease_until INTEGER NOT NULL DEFAULT 0,
    auto_last_digest TEXT,
    auto_last_window TEXT
);

CREATE TABLE webui_managed_assets (
    source_sha TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL
);

CREATE TABLE member_sync_pacing (
    bot_id INTEGER PRIMARY KEY CHECK (bot_id > 0),
    request_after_ms INTEGER NOT NULL CHECK (request_after_ms >= 0),
    lease_owner TEXT,
    lease_until_ms INTEGER,
    generation INTEGER NOT NULL DEFAULT 0 CHECK (generation >= 0),
    CHECK ((lease_owner IS NULL) = (lease_until_ms IS NULL))
);

CREATE INDEX idx_events_guild_id ON events(guild_id, id);

CREATE INDEX idx_events_guild_type_id ON events(guild_id, event_type, id);

CREATE INDEX idx_bans_guild_id ON bans(guild_id, id);

CREATE INDEX bot_actions_expiry ON bot_actions(state, expires_at);

CREATE UNIQUE INDEX idx_bans_one_active ON bans(guild_id, user_id) WHERE active = 1;

CREATE TABLE known_bad_images (
    guild_id INTEGER NOT NULL CHECK (guild_id > 0),
    hash TEXT NOT NULL CHECK (length(hash) = 64 AND hash NOT GLOB '*[^0-9a-f]*'),
    added_at TEXT NOT NULL,
    added_via TEXT NOT NULL CHECK (added_via = 'webui'),
    actor TEXT NOT NULL CHECK (actor = 'unknown'),
    note TEXT CHECK (length(note) <= 256),
    PRIMARY KEY (guild_id, hash)
);

CREATE TABLE gateway_ban_deliveries (
    id TEXT PRIMARY KEY NOT NULL,
    guild_id INTEGER NOT NULL,
    enrollment_id TEXT NOT NULL,
    user_id INTEGER NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('ban', 'unban', 'audit')),
    observed_at_ms INTEGER NOT NULL,
    payload TEXT NOT NULL,
    applied INTEGER NOT NULL DEFAULT 0 CHECK (applied IN (0, 1)),
    case_saved INTEGER NOT NULL CHECK (case_saved IN (0, 1)),
    ban_result TEXT
);

CREATE INDEX idx_gateway_ban_deliveries_pending
    ON gateway_ban_deliveries(applied, case_saved);

CREATE INDEX idx_gateway_ban_deliveries_subject
    ON gateway_ban_deliveries(guild_id, user_id, kind, observed_at_ms);

CREATE INDEX idx_bans_expiry ON bans(expires_at) WHERE active=1 AND expires_at IS NOT NULL;

INSERT INTO meta (key, value) VALUES
    ('schema_family', 'jmodguard-core-1'),
    ('config_version', '0'),
    ('action_request_floor', '0');
INSERT INTO component_updates (component) VALUES ('bot'), ('api'), ('webui');
