-- Transient latest-observed Discord message state. One row per scoped message.
-- The expiry is fixed from original creation; edit/restore never refreshes it.
CREATE TABLE guild_message_snapshots (
    guild_id INTEGER NOT NULL CHECK(guild_id > 0),
    enrollment_id TEXT NOT NULL,
    channel_id INTEGER NOT NULL CHECK(channel_id > 0),
    message_id INTEGER NOT NULL CHECK(message_id > 0),
    author_id INTEGER,
    author_name TEXT,
    content TEXT,
    content_truncated INTEGER NOT NULL DEFAULT 0 CHECK(content_truncated IN (0,1)),
    content_original_bytes INTEGER,
    attachments TEXT,
    attachments_truncated INTEGER NOT NULL DEFAULT 0 CHECK(attachments_truncated IN (0,1)),
    attachments_omitted INTEGER,
    created_at_ms INTEGER NOT NULL,
    first_observed_at_ms INTEGER NOT NULL,
    last_observed_at_ms INTEGER NOT NULL,
    edit_at_ms INTEGER,
    expires_at_ms INTEGER NOT NULL,
    generation INTEGER NOT NULL DEFAULT 1 CHECK(generation > 0),
    coverage_epoch INTEGER NOT NULL CHECK(coverage_epoch >= 0),
    continuous INTEGER NOT NULL DEFAULT 1 CHECK(continuous IN (0,1)),
    encoded_bytes INTEGER NOT NULL CHECK(encoded_bytes >= 0 AND encoded_bytes <= 65536),
    reservation_bytes INTEGER NOT NULL DEFAULT 0 CHECK(reservation_bytes >= 0 AND reservation_bytes <= 17000000),
    format_version INTEGER NOT NULL DEFAULT 1 CHECK(format_version = 1),
    PRIMARY KEY(guild_id,enrollment_id,channel_id,message_id),
    CHECK(expires_at_ms = created_at_ms + 86400000),
    CHECK(content IS NULL OR length(CAST(content AS BLOB)) <= 32768),
    CHECK(attachments IS NULL OR length(CAST(attachments AS BLOB)) <= 16384),
    CHECK(author_name IS NULL OR length(CAST(author_name AS BLOB)) <= 256)
);
CREATE INDEX guild_snapshot_expiry ON guild_message_snapshots(expires_at_ms,guild_id,enrollment_id,channel_id,message_id);

CREATE TRIGGER guild_snapshot_insert BEFORE INSERT ON guild_message_snapshots
WHEN NOT EXISTS (
    SELECT 1 FROM guild_memberships
    WHERE guild_id=NEW.guild_id AND enrollment_id=NEW.enrollment_id AND active=1
)
BEGIN SELECT RAISE(ABORT, 'guild membership is not active'); END;
CREATE TRIGGER guild_snapshot_update BEFORE UPDATE ON guild_message_snapshots
WHEN NEW.guild_id != OLD.guild_id OR NEW.enrollment_id != OLD.enrollment_id
    OR NEW.channel_id != OLD.channel_id OR NEW.message_id != OLD.message_id
    OR NEW.created_at_ms != OLD.created_at_ms OR NEW.expires_at_ms != OLD.expires_at_ms
    OR NOT EXISTS (
      SELECT 1 FROM guild_memberships
      WHERE guild_id=NEW.guild_id AND enrollment_id=NEW.enrollment_id AND active=1
    )
BEGIN SELECT RAISE(ABORT, 'guild snapshot scope or lifetime changed'); END;

-- Latest health only; counters are reset with a new epoch rather than a history log.
CREATE TABLE guild_snapshot_health (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    epoch INTEGER NOT NULL DEFAULT 0,
    state TEXT NOT NULL DEFAULT 'unknown',
    reason TEXT,
    last_observed_at_ms INTEGER,
    last_cleanup_at_ms INTEGER,
    lost_count INTEGER NOT NULL DEFAULT 0,
    snapshot_count INTEGER NOT NULL DEFAULT 0 CHECK(snapshot_count >= 0),
    snapshot_logical_bytes INTEGER NOT NULL DEFAULT 0 CHECK(snapshot_logical_bytes >= 0),
    expired_count INTEGER NOT NULL DEFAULT 0 CHECK(expired_count >= 0),
    evicted_count INTEGER NOT NULL DEFAULT 0 CHECK(evicted_count >= 0),
    counters_reset_at_ms INTEGER,
    estimated_snapshot_bytes INTEGER NOT NULL DEFAULT 0 CHECK(estimated_snapshot_bytes >= 0),
    physical_sample_bytes INTEGER CHECK(physical_sample_bytes IS NULL OR physical_sample_bytes >= 0),
    physical_sample_at_ms INTEGER,
    physical_debt_bytes INTEGER NOT NULL DEFAULT 0 CHECK(physical_debt_bytes >= 0),
    capacity_paused INTEGER NOT NULL DEFAULT 0 CHECK(capacity_paused IN (0,1)),
    updated_at_ms INTEGER
);
INSERT INTO guild_snapshot_health(singleton) VALUES(1);

CREATE TRIGGER guild_snapshot_account_insert AFTER INSERT ON guild_message_snapshots
BEGIN UPDATE guild_snapshot_health SET estimated_snapshot_bytes=estimated_snapshot_bytes+NEW.encoded_bytes+4096,
    snapshot_count=snapshot_count+1,snapshot_logical_bytes=snapshot_logical_bytes+NEW.encoded_bytes,
    physical_debt_bytes=MIN(4000000000,physical_debt_bytes+NEW.reservation_bytes) WHERE singleton=1; END;
CREATE TRIGGER guild_snapshot_account_update AFTER UPDATE ON guild_message_snapshots
BEGIN UPDATE guild_snapshot_health SET estimated_snapshot_bytes=estimated_snapshot_bytes+NEW.encoded_bytes-OLD.encoded_bytes,
    snapshot_logical_bytes=snapshot_logical_bytes+NEW.encoded_bytes-OLD.encoded_bytes,
    physical_debt_bytes=MIN(4000000000,physical_debt_bytes+NEW.reservation_bytes) WHERE singleton=1; END;
CREATE TRIGGER guild_snapshot_account_delete AFTER DELETE ON guild_message_snapshots
BEGIN UPDATE guild_snapshot_health SET estimated_snapshot_bytes=estimated_snapshot_bytes-OLD.encoded_bytes-4096,
    snapshot_count=snapshot_count-1,snapshot_logical_bytes=snapshot_logical_bytes-OLD.encoded_bytes WHERE singleton=1; END;

DROP TRIGGER guild_membership_delete;
CREATE TRIGGER guild_membership_delete BEFORE DELETE ON guild_memberships
WHEN OLD.active = 1
    OR EXISTS (SELECT 1 FROM guild_log_channels WHERE guild_id = OLD.guild_id)
    OR EXISTS (SELECT 1 FROM guild_cases WHERE guild_id = OLD.guild_id)
    OR EXISTS (SELECT 1 FROM guild_case_evidence WHERE guild_id = OLD.guild_id)
    OR EXISTS (SELECT 1 FROM guild_case_history WHERE guild_id = OLD.guild_id)
    OR EXISTS (SELECT 1 FROM guild_case_sources WHERE guild_id = OLD.guild_id)
    OR EXISTS (SELECT 1 FROM guild_bunny_evidence WHERE guild_id = OLD.guild_id)
    OR EXISTS (SELECT 1 FROM guild_tenant_settings WHERE guild_id = OLD.guild_id)
    OR EXISTS (SELECT 1 FROM guild_message_snapshots WHERE guild_id = OLD.guild_id)
BEGIN SELECT RAISE(ABORT, 'guild membership still has live data'); END;
