-- jModGuard schema. guild_id 0 is the GLOBAL scope (applies to every server);
-- a guild row with a matching key overrides the global value.

CREATE TABLE IF NOT EXISTS guild_settings (
    guild_id INTEGER NOT NULL PRIMARY KEY,
    settings TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS rules (
    guild_id INTEGER NOT NULL,
    rule_id  TEXT NOT NULL,
    data     TEXT NOT NULL,
    PRIMARY KEY (guild_id, rule_id)
);

CREATE TABLE IF NOT EXISTS commands (
    guild_id INTEGER NOT NULL,
    name     TEXT NOT NULL,
    data     TEXT NOT NULL,
    PRIMARY KEY (guild_id, name)
);

CREATE TABLE IF NOT EXISTS templates (
    guild_id   INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    template   TEXT NOT NULL,
    PRIMARY KEY (guild_id, event_type)
);

CREATE TABLE IF NOT EXISTS log_channels (
    guild_id   INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    channel_id INTEGER NOT NULL,
    PRIMARY KEY (guild_id, event_type)
);

CREATE TABLE IF NOT EXISTS badwords (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id INTEGER NOT NULL,
    pattern TEXT NOT NULL,
    note    TEXT
);
CREATE INDEX IF NOT EXISTS idx_badwords_guild ON badwords(guild_id);

CREATE TABLE IF NOT EXISTS events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id   INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    user_id    INTEGER,
    actor_id   INTEGER,
    data       TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_events_guild ON events(guild_id, created_at);
CREATE INDEX IF NOT EXISTS idx_events_user ON events(user_id);

CREATE TABLE IF NOT EXISTS warns (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id     INTEGER NOT NULL,
    user_id      INTEGER NOT NULL,
    moderator_id INTEGER NOT NULL,
    reason       TEXT,
    created_at   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_warns_user ON warns(guild_id, user_id);

CREATE TABLE IF NOT EXISTS bans (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id     INTEGER NOT NULL,
    user_id      INTEGER NOT NULL,
    moderator_id INTEGER,
    reason       TEXT,
    created_at   TEXT NOT NULL,
    active       INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_bans_guild ON bans(guild_id, user_id);

-- Fresh overhaul databases have a family marker from the same transaction as
-- their first tables. Unmarked/foreign databases must not be adopted implicitly.
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
INSERT INTO meta (key, value) VALUES ('schema_family', 'jmodguard-core-1');
