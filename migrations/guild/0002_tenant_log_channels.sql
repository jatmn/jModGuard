-- Inactive tenant data foundation. Initial membership only; no automatic rejoin/adoption.
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
CREATE TRIGGER guild_membership_no_delete BEFORE DELETE ON guild_memberships
BEGIN SELECT RAISE(ABORT,'guild membership requires coordinated cleanup'); END;

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
