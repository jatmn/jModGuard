-- Bot membership observations only; no queue/nickname binding or data erasure.
CREATE TABLE guild_enrollments (
    guild_id INTEGER PRIMARY KEY CHECK(guild_id > 0),
    id TEXT NOT NULL UNIQUE,
    joined_at_ns INTEGER NOT NULL CHECK(joined_at_ns > 0),
    active INTEGER NOT NULL CHECK(active IN (0,1))
);
