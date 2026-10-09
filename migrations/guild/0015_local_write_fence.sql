-- Guild-local write fence. Core fences live in another database, so a status
-- read must see this row inside its own transaction.
CREATE TABLE guild_local_write_fence (
    guild_id INTEGER PRIMARY KEY CHECK(guild_id > 0),
    fenced_at_ms INTEGER NOT NULL
);
