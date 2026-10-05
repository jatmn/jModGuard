-- Ordered guild pages and bounded action expiry polls.
CREATE INDEX idx_events_guild_id ON events(guild_id, id);
CREATE INDEX idx_events_guild_type_id ON events(guild_id, event_type, id);
CREATE INDEX idx_bans_guild_id ON bans(guild_id, id);
CREATE INDEX bot_actions_expiry ON bot_actions(state, expires_at);
