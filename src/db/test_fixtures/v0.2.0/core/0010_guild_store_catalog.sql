-- Nonsecret directory metadata only. No store is provisioned or selected at startup.
CREATE TABLE guild_store_catalog (
    store_id TEXT PRIMARY KEY NOT NULL
        CHECK(length(store_id) = 32 AND store_id NOT GLOB '*[^0-9a-f]*'),
    dedicated_guild_id INTEGER
        CHECK(dedicated_guild_id IS NULL OR (typeof(dedicated_guild_id) = 'integer' AND dedicated_guild_id > 0))
);

CREATE TABLE guild_store_placements (
    guild_id INTEGER PRIMARY KEY CHECK(guild_id > 0),
    store_id TEXT NOT NULL,
    revision TEXT NOT NULL
        CHECK(length(revision) = 32 AND revision NOT GLOB '*[^0-9a-f]*')
);
CREATE INDEX guild_store_placements_store ON guild_store_placements(store_id);

-- These checks do not depend on a connection-local foreign_keys pragma. Remote
-- transactions use their own streams. Validate both inserts and later updates.
CREATE TRIGGER guild_store_placement_insert BEFORE INSERT ON guild_store_placements
WHEN NOT EXISTS (
    SELECT 1 FROM guild_store_catalog WHERE store_id=NEW.store_id
    AND (dedicated_guild_id IS NULL OR dedicated_guild_id=NEW.guild_id)
)
BEGIN SELECT RAISE(ABORT, 'invalid guild store placement'); END;
CREATE TRIGGER guild_store_placement_update BEFORE UPDATE ON guild_store_placements
WHEN NOT EXISTS (
    SELECT 1 FROM guild_store_catalog WHERE store_id=NEW.store_id
    AND (dedicated_guild_id IS NULL OR dedicated_guild_id=NEW.guild_id)
)
BEGIN SELECT RAISE(ABORT, 'invalid guild store placement'); END;
-- REPLACE can delete/reinsert without firing delete triggers when recursive
-- triggers are disabled. Refuse duplicate-ID insertion before conflict handling.
CREATE TRIGGER guild_store_catalog_no_replace BEFORE INSERT ON guild_store_catalog
WHEN EXISTS (SELECT 1 FROM guild_store_catalog WHERE store_id=NEW.store_id)
BEGIN SELECT RAISE(ABORT, 'guild store identity is already registered'); END;
CREATE TRIGGER guild_store_catalog_immutable BEFORE UPDATE ON guild_store_catalog
BEGIN SELECT RAISE(ABORT, 'guild store identity and ownership are immutable'); END;
CREATE TRIGGER guild_store_catalog_referenced BEFORE DELETE ON guild_store_catalog
WHEN EXISTS (SELECT 1 FROM guild_store_placements WHERE store_id=OLD.store_id)
BEGIN SELECT RAISE(ABORT, 'guild store is still referenced'); END;
