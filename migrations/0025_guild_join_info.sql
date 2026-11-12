-- Owner server directory facts observed by the bot at membership time.
-- Display data only: these rows never authorize access and carry no secrets.
-- Rows are keyed by guild and retained after departure as directory history.
CREATE TABLE guild_join_info (
    guild_id INTEGER PRIMARY KEY CHECK(guild_id > 0),
    name TEXT NOT NULL CHECK(length(name) BETWEEN 1 AND 128),
    icon_hash TEXT CHECK(icon_hash IS NULL OR length(icon_hash) <= 128),
    member_count INTEGER CHECK(member_count IS NULL OR (typeof(member_count) = 'integer' AND member_count >= 0)),
    added_by TEXT CHECK(added_by IS NULL OR (typeof(added_by) = 'text' AND length(added_by) BETWEEN 1 AND 32 AND added_by NOT GLOB '*[^0-9]*')),
    added_by_name TEXT CHECK(added_by_name IS NULL OR length(added_by_name) <= 64),
    recorded_at_ns INTEGER NOT NULL CHECK(recorded_at_ns > 0)
);
