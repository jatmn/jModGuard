-- Guild-owned member directory. Canonical case payload stays in guild_cases.
-- Admission marker survives loss of a replaceable seed or job row. For the
-- migration, existing enrollments are explicitly eligible exactly once.
CREATE TABLE guild_member_feature (
    guild_id INTEGER PRIMARY KEY CHECK(guild_id > 0),
    enrollment_id TEXT NOT NULL,
    seed_admitted INTEGER NOT NULL DEFAULT 0 CHECK(seed_admitted IN (0,1))
);
INSERT INTO guild_member_feature(guild_id,enrollment_id,seed_admitted)
SELECT guild_id,enrollment_id,0 FROM guild_memberships WHERE active=1;
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
    case_backfill_done INTEGER NOT NULL DEFAULT 0 CHECK(case_backfill_done IN (0,1)),
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
    unseen_candidate_count INTEGER NOT NULL DEFAULT 0,
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
    provenance TEXT NOT NULL CHECK(provenance IN ('observed','imported','reconciled')),
    PRIMARY KEY(guild_id,enrollment_id,stay_id)
);
CREATE INDEX guild_member_stays_user ON guild_member_stays(guild_id,enrollment_id,user_id,joined_at_ms,stay_id);

-- A verified state is evidence of what was present at check time, not of the
-- date on which the change happened.
CREATE TABLE guild_member_state_observations (
    guild_id INTEGER NOT NULL,
    enrollment_id TEXT NOT NULL,
    observation_id TEXT NOT NULL,
    user_id INTEGER NOT NULL CHECK(user_id>0),
    kind TEXT NOT NULL CHECK(kind IN ('member_absent','ban_absent','membership_changed')),
    observed_at_ms INTEGER NOT NULL,
    PRIMARY KEY(guild_id,enrollment_id,observation_id)
);
CREATE INDEX guild_member_state_observations_user ON guild_member_state_observations(guild_id,enrollment_id,user_id,observed_at_ms,observation_id);

-- Specific uncertain users are checked under the same global Discord request
-- gate. A missing Gateway join token must never close a newer stay by guess.
CREATE TABLE guild_member_target_checks (
    guild_id INTEGER NOT NULL,
    enrollment_id TEXT NOT NULL,
    user_id INTEGER NOT NULL CHECK(user_id>0),
    kind TEXT NOT NULL CHECK(kind IN ('member','ban')),
    reason TEXT NOT NULL,
    row_revision INTEGER,
    requested_at_ms INTEGER NOT NULL,
    next_attempt_at_ms INTEGER NOT NULL,
    PRIMARY KEY(guild_id,enrollment_id,user_id,kind)
);
CREATE INDEX guild_member_target_checks_due ON guild_member_target_checks(guild_id,enrollment_id,next_attempt_at_ms);

-- Completed initial import never reopens. This independent checkpoint admits
-- only incremental audit recovery after uncertain Gateway coverage.
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
CREATE TRIGGER guild_member_case_retain AFTER INSERT ON guild_cases
WHEN NEW.deleted=0 AND json_valid(NEW.payload)
 AND json_extract(NEW.payload,'$.action') IN ('ban','unban')
 AND json_extract(NEW.payload,'$.outcome')='succeeded'
BEGIN
 INSERT INTO guild_member_directory(guild_id,enrollment_id,user_id,ever_banned,observed_at_ms)
 VALUES(NEW.guild_id,NEW.enrollment_id,CAST(json_extract(NEW.payload,'$.subject_id') AS INTEGER),1,CAST(json_extract(NEW.payload,'$.occurred_at_ms') AS INTEGER))
 ON CONFLICT(guild_id,enrollment_id,user_id) DO UPDATE SET ever_banned=1,cleanup_after_ms=NULL,revision=revision+1;
 UPDATE guild_member_seed SET directory_revision=directory_revision+1 WHERE guild_id=NEW.guild_id AND enrollment_id=NEW.enrollment_id;
END;
CREATE TRIGGER guild_member_case_remove AFTER UPDATE OF deleted ON guild_cases
WHEN NEW.deleted=1
BEGIN DELETE FROM guild_member_case_index WHERE guild_id=NEW.guild_id AND enrollment_id=NEW.enrollment_id AND case_id=NEW.case_id; END;
CREATE TRIGGER guild_member_case_purge AFTER DELETE ON guild_cases
BEGIN DELETE FROM guild_member_case_index WHERE guild_id=OLD.guild_id AND enrollment_id=OLD.enrollment_id AND case_id=OLD.case_id; END;

DROP TRIGGER guild_membership_delete;
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
 OR EXISTS(SELECT 1 FROM guild_member_state_observations WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_target_checks WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_audit_recovery WHERE guild_id=OLD.guild_id)
 OR EXISTS(SELECT 1 FROM guild_member_feature WHERE guild_id=OLD.guild_id)
BEGIN SELECT RAISE(ABORT,'guild membership still has live data'); END;
