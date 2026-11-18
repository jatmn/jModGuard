-- Discord channel IDs are owned by one guild and must not act as cross-guild
-- defaults. Preserve legacy mappings for operators to inspect, then remove
-- them from the active routing table so upgrades do not keep sending another
-- guild's moderation events to that channel.
CREATE TABLE IF NOT EXISTS legacy_global_log_channels (
    event_type TEXT PRIMARY KEY,
    channel_id INTEGER NOT NULL
);

INSERT OR IGNORE INTO legacy_global_log_channels (event_type, channel_id)
SELECT event_type, channel_id
FROM log_channels
WHERE guild_id = 0;

DELETE FROM log_channels WHERE guild_id = 0;
