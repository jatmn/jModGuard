-- Owner server directory facts observed by the bot at membership time.
-- Display data only: these rows never authorize access and carry no secrets.
-- Rows are keyed by guild and retained after departure as directory history.
-- Name/icon/member count describe the guild (recorded_at_ns is their observation
-- time); the adder describes one membership and carries that membership's join time.
CREATE TABLE guild_join_info (
    guild_id INTEGER PRIMARY KEY CHECK(guild_id > 0),
    name TEXT NOT NULL CHECK(length(name) BETWEEN 1 AND 128),
    icon_hash TEXT CHECK(icon_hash IS NULL OR length(icon_hash) <= 128),
    member_count INTEGER CHECK(member_count IS NULL OR (typeof(member_count) = 'integer' AND member_count >= 0)),
    added_by TEXT CHECK(added_by IS NULL OR (typeof(added_by) = 'text' AND length(added_by) BETWEEN 1 AND 32 AND added_by NOT GLOB '*[^0-9]*')),
    added_by_name TEXT CHECK(added_by_name IS NULL OR length(added_by_name) <= 64),
    added_by_joined_at_ns INTEGER CHECK(added_by_joined_at_ns IS NULL OR (typeof(added_by_joined_at_ns) = 'integer' AND added_by_joined_at_ns > 0)),
    recorded_at_ns INTEGER NOT NULL CHECK(recorded_at_ns > 0),
    CHECK((added_by IS NULL) = (added_by_joined_at_ns IS NULL))
);
