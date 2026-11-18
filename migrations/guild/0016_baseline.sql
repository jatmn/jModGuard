-- Fresh guild schema at v0.2.0 (user_version 17).
-- The initializer stamps user_version in the same transaction.

CREATE TABLE store_identity (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    schema_family TEXT NOT NULL CHECK(schema_family='jmodguard-guild-1'),
    store_id TEXT NOT NULL CHECK(length(CAST(store_id AS BLOB))=32 AND store_id NOT GLOB '*[^0-9a-f]*'),
    dedicated_guild_id INTEGER CHECK(dedicated_guild_id IS NULL OR
        (typeof(dedicated_guild_id)='integer' AND dedicated_guild_id>0))
);

CREATE TRIGGER store_identity_no_replace BEFORE INSERT ON store_identity
WHEN EXISTS(SELECT 1 FROM store_identity)
BEGIN SELECT RAISE(ABORT,'store identity already exists'); END;

CREATE TRIGGER store_identity_no_update BEFORE UPDATE ON store_identity
BEGIN SELECT RAISE(ABORT,'store identity is immutable'); END;

CREATE TRIGGER store_identity_no_delete BEFORE DELETE ON store_identity
BEGIN SELECT RAISE(ABORT,'store identity is immutable'); END;

CREATE TABLE guild_bindings (
    guild_id INTEGER PRIMARY KEY CHECK(guild_id>0),
    revision TEXT NOT NULL CHECK(length(CAST(revision AS BLOB))=32 AND revision NOT GLOB '*[^0-9a-f]*')
);

CREATE TRIGGER guild_binding_owner BEFORE INSERT ON guild_bindings
WHEN NOT EXISTS(SELECT 1 FROM store_identity WHERE singleton=1 AND
    (dedicated_guild_id IS NULL OR dedicated_guild_id=NEW.guild_id))
BEGIN SELECT RAISE(ABORT,'invalid guild store owner'); END;

CREATE TRIGGER guild_binding_no_replace BEFORE INSERT ON guild_bindings
WHEN EXISTS(SELECT 1 FROM guild_bindings WHERE guild_id=NEW.guild_id)
BEGIN SELECT RAISE(ABORT,'guild binding already exists'); END;

CREATE TRIGGER guild_binding_no_update BEFORE UPDATE ON guild_bindings
BEGIN SELECT RAISE(ABORT,'guild binding is immutable'); END;

CREATE TABLE mutation_receipts (
    receipt_id TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL
);

CREATE INDEX mutation_receipts_created_at ON mutation_receipts(created_at);

CREATE TABLE guild_memberships (
    guild_id INTEGER PRIMARY KEY CHECK(guild_id>0),
    enrollment_id TEXT NOT NULL CHECK(length(CAST(enrollment_id AS BLOB))=32 AND enrollment_id NOT GLOB '*[^0-9a-f]*'),
    joined_at_ns INTEGER NOT NULL CHECK(typeof(joined_at_ns)='integer' AND joined_at_ns>0),
    active INTEGER NOT NULL CHECK(active IN (0,1)),
    config_revision TEXT NOT NULL CHECK(length(CAST(config_revision AS BLOB))=32 AND config_revision NOT GLOB '*[^0-9a-f]*')
);

CREATE TRIGGER guild_membership_binding BEFORE INSERT ON guild_memberships
WHEN NOT EXISTS(SELECT 1 FROM guild_bindings WHERE guild_id=NEW.guild_id)
BEGIN SELECT RAISE(ABORT,'guild placement is unbound'); END;

CREATE TRIGGER guild_membership_no_replace BEFORE INSERT ON guild_memberships
WHEN EXISTS(SELECT 1 FROM guild_memberships WHERE guild_id=NEW.guild_id)
BEGIN SELECT RAISE(ABORT,'guild membership already exists'); END;

CREATE TRIGGER guild_membership_update BEFORE UPDATE ON guild_memberships
WHEN NEW.guild_id!=OLD.guild_id OR NEW.enrollment_id!=OLD.enrollment_id OR
    NEW.joined_at_ns!=OLD.joined_at_ns OR (OLD.active=0 AND NEW.active!=0)
BEGIN SELECT RAISE(ABORT,'guild membership cannot be rebound'); END;

CREATE TABLE guild_log_channels (
    guild_id INTEGER NOT NULL CHECK(guild_id>0),
    enrollment_id TEXT NOT NULL,
    event_type TEXT NOT NULL CHECK(event_type IN ('member_join','member_leave','member_ban','member_unban','member_kick','message_edit','message_delete','message_bulk_delete','automod','mod_action','timeout_add','timeout_remove','plan_executed','system')),
    channel_id INTEGER NOT NULL CHECK(typeof(channel_id)='integer' AND channel_id>0),
    PRIMARY KEY(guild_id,event_type)
);

CREATE TRIGGER guild_log_channel_insert BEFORE INSERT ON guild_log_channels
WHEN NOT EXISTS(SELECT 1 FROM guild_memberships WHERE guild_id=NEW.guild_id AND enrollment_id=NEW.enrollment_id AND active=1)
BEGIN SELECT RAISE(ABORT,'guild membership is not active'); END;

CREATE TRIGGER guild_log_channel_update BEFORE UPDATE ON guild_log_channels
WHEN NEW.guild_id!=OLD.guild_id OR NEW.enrollment_id!=OLD.enrollment_id OR NEW.event_type!=OLD.event_type OR
    NOT EXISTS(SELECT 1 FROM guild_memberships WHERE guild_id=NEW.guild_id AND enrollment_id=NEW.enrollment_id AND active=1)
BEGIN SELECT RAISE(ABORT,'guild log channel scope changed'); END;

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

CREATE TRIGGER guild_case_delete BEFORE DELETE ON guild_cases
WHEN NOT EXISTS(SELECT 1 FROM guild_memberships WHERE guild_id=OLD.guild_id AND enrollment_id=OLD.enrollment_id AND active=0)
BEGIN SELECT RAISE(ABORT,'case replay keys require closed membership cleanup'); END;

CREATE INDEX guild_cases_visible ON guild_cases(guild_id,enrollment_id,deleted,case_id);

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
    content_supplied INTEGER NOT NULL DEFAULT 0 CHECK(content_supplied IN (0,1)),
    attachments_supplied INTEGER NOT NULL DEFAULT 0 CHECK(attachments_supplied IN (0,1)),
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

CREATE TABLE guild_member_feature (
    guild_id INTEGER PRIMARY KEY CHECK(guild_id > 0),
    enrollment_id TEXT NOT NULL,
    seed_admitted INTEGER NOT NULL DEFAULT 0 CHECK(seed_admitted IN (0,1))
);

CREATE TRIGGER guild_member_feature_identity BEFORE UPDATE ON guild_member_feature
WHEN NEW.guild_id!=OLD.guild_id OR NEW.enrollment_id!=OLD.enrollment_id OR NEW.seed_admitted<OLD.seed_admitted
BEGIN SELECT RAISE(ABORT,'member feature admission is immutable'); END;

CREATE TABLE guild_member_seed (
    guild_id INTEGER PRIMARY KEY CHECK(guild_id > 0),
    enrollment_id TEXT NOT NULL,
    bot_id INTEGER NOT NULL CHECK(bot_id > 0),
    initialized_at_ms INTEGER NOT NULL CHECK(initialized_at_ms > 0),
    initial_completed_at_ms INTEGER,
    reconciliation_needed INTEGER NOT NULL DEFAULT 0 CHECK(reconciliation_needed IN (0,1)),
    reconciliation_reason TEXT,
    gap_at_ms INTEGER,
    directory_revision INTEGER NOT NULL DEFAULT 0,
    case_backfill_after TEXT,
    case_backfill_done INTEGER NOT NULL DEFAULT 0 CHECK(case_backfill_done IN (0,1)), gap_generation INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY(guild_id) REFERENCES guild_bindings(guild_id)
);

CREATE TRIGGER guild_member_seed_insert BEFORE INSERT ON guild_member_seed
WHEN NOT EXISTS (SELECT 1 FROM guild_memberships WHERE guild_id=NEW.guild_id AND enrollment_id=NEW.enrollment_id AND active=1)
BEGIN SELECT RAISE(ABORT,'member seed scope is not active'); END;

CREATE TRIGGER guild_member_seed_identity BEFORE UPDATE ON guild_member_seed
WHEN NEW.guild_id!=OLD.guild_id OR NEW.enrollment_id!=OLD.enrollment_id OR NEW.bot_id!=OLD.bot_id
 OR NEW.initialized_at_ms!=OLD.initialized_at_ms
 OR (OLD.initial_completed_at_ms IS NOT NULL AND NEW.initial_completed_at_ms IS NOT OLD.initial_completed_at_ms)
BEGIN SELECT RAISE(ABORT,'member seed identity/completion is immutable'); END;

CREATE TABLE guild_member_sync (
    guild_id INTEGER PRIMARY KEY CHECK(guild_id > 0),
    enrollment_id TEXT NOT NULL,
    bot_id INTEGER NOT NULL CHECK(bot_id > 0),
    run_id TEXT NOT NULL,
    run_kind TEXT NOT NULL CHECK(run_kind IN ('initial','reconcile')),
    phase TEXT NOT NULL CHECK(phase IN ('members','bans','verify','audit','complete')),
    status TEXT NOT NULL CHECK(status IN ('queued','running','waiting','paused','needs_attention','complete')),
    status_reason TEXT,
    cursor TEXT,
    processed INTEGER NOT NULL DEFAULT 0,
    inserted INTEGER NOT NULL DEFAULT 0,
    updated INTEGER NOT NULL DEFAULT 0,
    unchanged INTEGER NOT NULL DEFAULT 0,
    total_estimate INTEGER,
    started_at_ms INTEGER NOT NULL,
    last_progress_at_ms INTEGER,
    completed_at_ms INTEGER,
    next_attempt_at_ms INTEGER,
    last_success_at_ms INTEGER,
    progress_revision INTEGER NOT NULL DEFAULT 0,
    generation INTEGER NOT NULL DEFAULT 0,
    lease_owner TEXT,
    lease_until_ms INTEGER,
    request_after_ms INTEGER,
    last_operation_key TEXT,
    page_size INTEGER NOT NULL DEFAULT 100,
    pace_ms INTEGER NOT NULL DEFAULT 15000,
    active_guild_count INTEGER NOT NULL DEFAULT 1,
    unseen_candidate_count INTEGER NOT NULL DEFAULT 0, last_success_processed INTEGER, last_success_inserted INTEGER, last_success_updated INTEGER, last_success_unchanged INTEGER, reconcile_gap_generation INTEGER, member_processed INTEGER, ban_processed INTEGER, verify_processed INTEGER, audit_processed INTEGER, member_page_interval_ms INTEGER,
    FOREIGN KEY(guild_id) REFERENCES guild_member_seed(guild_id)
);

CREATE INDEX guild_member_sync_due ON guild_member_sync(status,next_attempt_at_ms,lease_until_ms);

CREATE TABLE guild_member_directory (
    guild_id INTEGER NOT NULL CHECK(guild_id > 0),
    enrollment_id TEXT NOT NULL,
    user_id INTEGER NOT NULL CHECK(user_id > 0),
    username TEXT,
    global_name TEXT,
    nickname TEXT,
    search_text TEXT NOT NULL DEFAULT '',
    is_current INTEGER NOT NULL DEFAULT 0 CHECK(is_current IN (0,1)),
    is_banned INTEGER NOT NULL DEFAULT 0 CHECK(is_banned IN (0,1)),
    ever_banned INTEGER NOT NULL DEFAULT 0 CHECK(ever_banned IN (0,1)),
    current_ban_reason TEXT,
    joined_at_ms INTEGER,
    departed_at_ms INTEGER,
    current_stay_id TEXT,
    cleanup_after_ms INTEGER,
    cleanup_member_checked_at_ms INTEGER,
    observed_at_ms INTEGER NOT NULL,
    ban_observed_at_ms INTEGER,
    revision INTEGER NOT NULL DEFAULT 0,
    last_seen_run TEXT,
    last_ban_seen_run TEXT,
    PRIMARY KEY(guild_id,enrollment_id,user_id)
);

CREATE INDEX guild_member_status_join ON guild_member_directory(guild_id,enrollment_id,is_current,is_banned,ever_banned,joined_at_ms,user_id);

CREATE INDEX guild_member_join ON guild_member_directory(guild_id,enrollment_id,joined_at_ms,user_id);

CREATE INDEX guild_member_cleanup ON guild_member_directory(guild_id,enrollment_id,cleanup_after_ms,user_id);

CREATE TABLE guild_member_pending_bans (
    guild_id INTEGER NOT NULL,
    enrollment_id TEXT NOT NULL,
    user_id INTEGER NOT NULL CHECK(user_id>0),
    operation_id TEXT NOT NULL,
    started_at_ms INTEGER NOT NULL,
    retry_after_ms INTEGER NOT NULL,
    PRIMARY KEY(guild_id,enrollment_id,user_id,operation_id)
);

CREATE INDEX guild_member_pending_user ON guild_member_pending_bans(guild_id,enrollment_id,user_id);

CREATE TABLE guild_member_stays (
    guild_id INTEGER NOT NULL,
    enrollment_id TEXT NOT NULL,
    stay_id TEXT NOT NULL,
    user_id INTEGER NOT NULL CHECK(user_id > 0),
    joined_at_ms INTEGER,
    departed_at_ms INTEGER,
    ended_unknown INTEGER NOT NULL DEFAULT 0 CHECK(ended_unknown IN (0,1)),
    provenance TEXT NOT NULL CHECK(provenance IN ('observed','imported','reconciled','observed_rejoin','imported_rejoin','reconciled_rejoin')),
    PRIMARY KEY(guild_id,enrollment_id,stay_id)
);

CREATE INDEX guild_member_stays_user ON guild_member_stays(guild_id,enrollment_id,user_id,joined_at_ms,stay_id);

CREATE TABLE guild_member_target_checks (
    guild_id INTEGER NOT NULL,
    enrollment_id TEXT NOT NULL,
    user_id INTEGER NOT NULL CHECK(user_id>0),
    kind TEXT NOT NULL CHECK(kind IN ('member','ban')),
    reason TEXT NOT NULL,
    row_revision INTEGER,
    expected_stay_id TEXT,
    requested_at_ms INTEGER NOT NULL,
    next_attempt_at_ms INTEGER NOT NULL,
    PRIMARY KEY(guild_id,enrollment_id,user_id,kind)
);

CREATE INDEX guild_member_target_checks_due ON guild_member_target_checks(guild_id,enrollment_id,next_attempt_at_ms);

CREATE TABLE guild_member_audit_recovery (
    guild_id INTEGER PRIMARY KEY,
    enrollment_id TEXT NOT NULL,
    bot_id INTEGER NOT NULL,
    run_id TEXT NOT NULL,
    started_at_ms INTEGER NOT NULL,
    cursor TEXT,
    status TEXT NOT NULL CHECK(status IN ('queued','running','waiting','complete','needs_attention')),
    status_reason TEXT,
    generation INTEGER NOT NULL DEFAULT 0,
    lease_owner TEXT,
    lease_until_ms INTEGER,
    next_attempt_at_ms INTEGER,
    request_after_ms INTEGER,
    processed INTEGER NOT NULL DEFAULT 0,
    last_progress_at_ms INTEGER,
    restart_requested_at_ms INTEGER
);

CREATE TABLE guild_member_case_index (
    guild_id INTEGER NOT NULL,
    enrollment_id TEXT NOT NULL,
    user_id INTEGER NOT NULL CHECK(user_id > 0),
    case_id TEXT NOT NULL,
    action_kind TEXT NOT NULL,
    occurred_at_ms INTEGER,
    PRIMARY KEY(guild_id,enrollment_id,case_id)
);

CREATE INDEX guild_member_cases_user ON guild_member_case_index(guild_id,enrollment_id,user_id,occurred_at_ms,case_id);

CREATE TRIGGER guild_member_case_insert AFTER INSERT ON guild_cases
WHEN NEW.deleted=0 AND json_valid(NEW.payload) AND json_extract(NEW.payload,'$.subject_id') IS NOT NULL AND json_extract(NEW.payload,'$.action') IS NOT NULL
BEGIN
 INSERT OR IGNORE INTO guild_member_case_index(guild_id,enrollment_id,user_id,case_id,action_kind,occurred_at_ms)
 VALUES(NEW.guild_id,NEW.enrollment_id,CAST(json_extract(NEW.payload,'$.subject_id') AS INTEGER),NEW.case_id,json_extract(NEW.payload,'$.action'),CAST(json_extract(NEW.payload,'$.occurred_at_ms') AS INTEGER));
END;

CREATE TRIGGER guild_member_case_remove AFTER UPDATE OF deleted ON guild_cases
WHEN NEW.deleted=1
BEGIN DELETE FROM guild_member_case_index WHERE guild_id=NEW.guild_id AND enrollment_id=NEW.enrollment_id AND case_id=NEW.case_id; END;

CREATE TRIGGER guild_member_case_purge AFTER DELETE ON guild_cases
BEGIN DELETE FROM guild_member_case_index WHERE guild_id=OLD.guild_id AND enrollment_id=OLD.enrollment_id AND case_id=OLD.case_id; END;

CREATE TABLE "guild_member_state_observations" (
    guild_id INTEGER NOT NULL,
    enrollment_id TEXT NOT NULL,
    observation_id TEXT NOT NULL,
    user_id INTEGER NOT NULL CHECK(user_id>0),
    kind TEXT NOT NULL CHECK(kind IN ('member_absent','ban_absent','ban_present','membership_changed')),
    observed_at_ms INTEGER NOT NULL,
    PRIMARY KEY(guild_id,enrollment_id,observation_id)
);

CREATE INDEX guild_member_state_observations_user
ON guild_member_state_observations(guild_id,enrollment_id,user_id,observed_at_ms,observation_id);

CREATE TABLE guild_member_unresolved_audits (
    guild_id INTEGER NOT NULL CHECK(guild_id>0),
    enrollment_id TEXT NOT NULL,
    entry_id INTEGER NOT NULL CHECK(entry_id>0),
    user_id INTEGER NOT NULL CHECK(user_id>0),
    action TEXT NOT NULL CHECK(action IN ('ban','unban')),
    occurred_at_ms INTEGER NOT NULL CHECK(occurred_at_ms>0),
    executor_id INTEGER CHECK(executor_id IS NULL OR executor_id>0),
    reason TEXT CHECK(reason IS NULL OR length(CAST(reason AS BLOB))<=4096),
    PRIMARY KEY(guild_id,enrollment_id,entry_id)
);

CREATE INDEX guild_member_unresolved_audits_user
ON guild_member_unresolved_audits(guild_id,enrollment_id,user_id,occurred_at_ms,entry_id);

CREATE TRIGGER guild_member_unresolved_audit_insert BEFORE INSERT ON guild_member_unresolved_audits
WHEN NOT EXISTS(SELECT 1 FROM guild_memberships m WHERE m.guild_id=NEW.guild_id AND m.enrollment_id=NEW.enrollment_id AND m.active=1)
 OR EXISTS(SELECT 1 FROM guild_case_sources s WHERE s.guild_id=NEW.guild_id AND s.enrollment_id=NEW.enrollment_id AND s.source_key=('discord:' || NEW.entry_id))
BEGIN SELECT RAISE(ABORT,'unresolved audit scope or source conflicts'); END;

CREATE TRIGGER guild_member_unresolved_audit_update BEFORE UPDATE ON guild_member_unresolved_audits
BEGIN SELECT RAISE(ABORT,'unresolved audit facts are immutable'); END;

CREATE TRIGGER guild_member_unresolved_audit_delete BEFORE DELETE ON guild_member_unresolved_audits
WHEN NOT EXISTS(SELECT 1 FROM guild_case_sources s WHERE s.guild_id=OLD.guild_id AND s.enrollment_id=OLD.enrollment_id AND s.source_key=('discord:' || OLD.entry_id))
 AND NOT EXISTS(SELECT 1 FROM guild_memberships m WHERE m.guild_id=OLD.guild_id AND m.enrollment_id=OLD.enrollment_id AND m.active=0)
BEGIN SELECT RAISE(ABORT,'unresolved audit requires exact source or closed-scope erasure'); END;

CREATE TRIGGER guild_member_case_retain AFTER INSERT ON guild_cases
WHEN NEW.deleted=0 AND json_valid(NEW.payload)
 AND json_extract(NEW.payload,'$.outcome')='succeeded'
 AND (json_extract(NEW.payload,'$.action')='ban'
      OR (json_extract(NEW.payload,'$.action')='unban'
          AND (NEW.source_key LIKE 'discord:%'
               OR json_type(NEW.payload,'$.source.gateway_observation') IS NOT NULL)))
BEGIN
 INSERT INTO guild_member_directory(guild_id,enrollment_id,user_id,ever_banned,observed_at_ms)
 VALUES(NEW.guild_id,NEW.enrollment_id,CAST(json_extract(NEW.payload,'$.subject_id') AS INTEGER),1,CAST(json_extract(NEW.payload,'$.occurred_at_ms') AS INTEGER))
 ON CONFLICT(guild_id,enrollment_id,user_id) DO UPDATE SET ever_banned=1,cleanup_after_ms=NULL,revision=revision+1;
END;

CREATE TABLE guild_local_write_fence (
    guild_id INTEGER PRIMARY KEY CHECK(guild_id > 0),
    fenced_at_ms INTEGER NOT NULL
);

CREATE TABLE guild_writer_epoch (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    generation INTEGER NOT NULL CHECK(generation > 0)
);

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

CREATE TABLE guild_member_pending_rejoin_notes (
    guild_id INTEGER NOT NULL CHECK(guild_id > 0),
    enrollment_id TEXT NOT NULL,
    user_id INTEGER NOT NULL CHECK(user_id > 0),
    stay_id TEXT NOT NULL,
    joined_at_ms INTEGER,
    PRIMARY KEY(guild_id, enrollment_id, user_id),
    FOREIGN KEY(guild_id, enrollment_id, stay_id) REFERENCES guild_member_stays(guild_id, enrollment_id, stay_id)
);

CREATE INDEX guild_member_pending_rejoin_stay
    ON guild_member_pending_rejoin_notes(guild_id, enrollment_id, stay_id);

CREATE TRIGGER guild_membership_delete BEFORE DELETE ON guild_memberships
WHEN OLD.active=1
 OR EXISTS(SELECT 1 FROM guild_log_channels WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_cases WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_case_evidence WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_case_history WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_case_sources WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_bunny_evidence WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_tenant_settings WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_seed WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_sync WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_directory WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_stays WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_case_index WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_pending_bans WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_pending_rejoin_notes WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_state_observations WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_target_checks WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_audit_recovery WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_unresolved_audits WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_feature WHERE guild_id=OLD.guild_id)
BEGIN SELECT RAISE(ABORT,'guild membership still has live data'); END;

INSERT INTO guild_snapshot_health (singleton) VALUES (1);
