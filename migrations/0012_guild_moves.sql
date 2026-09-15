-- Per-guild write fence for populated-store moves. Not a restore fence and
-- not a departed-guild ledger. Placement cutover still uses guild_store_placements.
CREATE TABLE guild_write_fences (
    guild_id INTEGER PRIMARY KEY CHECK(guild_id > 0),
    generation TEXT NOT NULL
        CHECK(length(generation) = 32 AND generation NOT GLOB '*[^0-9a-f]*'),
    phase TEXT NOT NULL CHECK(phase IN ('freeze', 'copy', 'verify', 'switch')),
    destination_store_id TEXT NOT NULL
        CHECK(length(destination_store_id) = 32 AND destination_store_id NOT GLOB '*[^0-9a-f]*'),
    destination_revision TEXT NOT NULL
        CHECK(length(destination_revision) = 32 AND destination_revision NOT GLOB '*[^0-9a-f]*'),
    created_at INTEGER NOT NULL
);
CREATE TRIGGER guild_write_fence_no_replace BEFORE INSERT ON guild_write_fences
WHEN EXISTS (SELECT 1 FROM guild_write_fences WHERE guild_id = NEW.guild_id)
BEGIN SELECT RAISE(ABORT, 'guild move already fenced'); END;
CREATE TRIGGER guild_write_fence_destination BEFORE INSERT ON guild_write_fences
WHEN NOT EXISTS (
    SELECT 1 FROM guild_store_catalog WHERE store_id = NEW.destination_store_id
    AND (dedicated_guild_id IS NULL OR dedicated_guild_id = NEW.guild_id)
)
BEGIN SELECT RAISE(ABORT, 'invalid guild move destination'); END;
