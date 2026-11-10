CREATE TABLE known_spam (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id INTEGER NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('sample', 'url')),
    value TEXT NOT NULL,
    match_key TEXT NOT NULL,
    note TEXT,
    UNIQUE (guild_id, kind, match_key)
);
CREATE INDEX idx_known_spam_scope ON known_spam(guild_id, id);
