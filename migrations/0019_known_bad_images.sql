-- Operator-managed digests only. Never attachment addresses or image payloads.
CREATE TABLE known_bad_images (
    guild_id INTEGER NOT NULL CHECK (guild_id > 0),
    hash TEXT NOT NULL CHECK (length(hash) = 64 AND hash NOT GLOB '*[^0-9a-f]*'),
    added_at TEXT NOT NULL,
    added_via TEXT NOT NULL CHECK (added_via = 'webui'),
    actor TEXT NOT NULL CHECK (actor = 'unknown'),
    note TEXT CHECK (length(note) <= 256),
    PRIMARY KEY (guild_id, hash)
);
