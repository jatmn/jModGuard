-- Daily per-guild membership counters for the analytics trends (joins/leaves).
-- One row per guild per day and no user IDs: this is not member/user activity
-- collection. The bot prunes rows beyond the analytics horizon with its own
-- counter writes, so the table stays bounded per guild.
CREATE TABLE analytics_member_daily (
    guild_id INTEGER NOT NULL CHECK(guild_id > 0),
    day      TEXT NOT NULL
        CHECK(length(day) = 10 AND day GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'),
    joins    INTEGER NOT NULL CHECK(joins >= 0),
    leaves   INTEGER NOT NULL CHECK(leaves >= 0),
    PRIMARY KEY (guild_id, day)
);

-- Time-ordered scans for the analytics daily series. Warn/ban rows are
-- low-frequency moderator actions; events already carries
-- idx_events_guild(guild_id, created_at). Platform-wide window scans keep
-- counting work growing with retained records, matching the statistics reads.
CREATE INDEX idx_analytics_warns_time ON warns(guild_id, created_at);
CREATE INDEX idx_analytics_bans_time ON bans(guild_id, created_at);
