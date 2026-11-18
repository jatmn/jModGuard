-- Separate guild-store lineage. Never applied by core/bot bootstrap.
CREATE TABLE store_identity (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    schema_family TEXT NOT NULL CHECK(schema_family='jmodguard-guild-1'),
    store_id TEXT NOT NULL CHECK(length(CAST(store_id AS BLOB))=32 AND store_id NOT GLOB '*[^0-9a-f]*'),
    dedicated_guild_id INTEGER CHECK(dedicated_guild_id IS NULL OR
        (typeof(dedicated_guild_id)='integer' AND dedicated_guild_id>0))
);
CREATE TRIGGER store_identity_no_replace BEFORE INSERT ON store_identity
WHEN EXISTS(SELECT 1 FROM store_identity)
BEGIN SELECT RAISE(ABORT,'store identity already exists'); END;
CREATE TRIGGER store_identity_no_update BEFORE UPDATE ON store_identity
BEGIN SELECT RAISE(ABORT,'store identity is immutable'); END;
CREATE TRIGGER store_identity_no_delete BEFORE DELETE ON store_identity
BEGIN SELECT RAISE(ABORT,'store identity is immutable'); END;

CREATE TABLE guild_bindings (
    guild_id INTEGER PRIMARY KEY CHECK(guild_id>0),
    revision TEXT NOT NULL CHECK(length(CAST(revision AS BLOB))=32 AND revision NOT GLOB '*[^0-9a-f]*')
);
CREATE TRIGGER guild_binding_owner BEFORE INSERT ON guild_bindings
WHEN NOT EXISTS(SELECT 1 FROM store_identity WHERE singleton=1 AND
    (dedicated_guild_id IS NULL OR dedicated_guild_id=NEW.guild_id))
BEGIN SELECT RAISE(ABORT,'invalid guild store owner'); END;
CREATE TRIGGER guild_binding_no_replace BEFORE INSERT ON guild_bindings
WHEN EXISTS(SELECT 1 FROM guild_bindings WHERE guild_id=NEW.guild_id)
BEGIN SELECT RAISE(ABORT,'guild binding already exists'); END;
CREATE TRIGGER guild_binding_no_update BEFORE UPDATE ON guild_bindings
BEGIN SELECT RAISE(ABORT,'guild binding is immutable'); END;
CREATE TRIGGER guild_binding_no_delete BEFORE DELETE ON guild_bindings
BEGIN SELECT RAISE(ABORT,'guild binding is immutable'); END;

-- Same receipt contract as the shared transport. No app config/version rows.
CREATE TABLE mutation_receipts (
    receipt_id TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL
);
CREATE INDEX mutation_receipts_created_at ON mutation_receipts(created_at);
