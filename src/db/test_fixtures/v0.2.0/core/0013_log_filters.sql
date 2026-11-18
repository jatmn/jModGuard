-- Event-specific, guild-local report exclusions. Filters survive clearing a
-- destination channel and are removed with the guild's directory data.
CREATE TABLE log_filters (
    guild_id INTEGER NOT NULL CHECK(guild_id > 0),
    event_type TEXT NOT NULL,
    data TEXT NOT NULL,
    PRIMARY KEY (guild_id, event_type)
);
