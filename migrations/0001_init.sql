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

CREATE TABLE IF NOT EXISTS members (
    guild_id        INTEGER NOT NULL,
    user_id         INTEGER NOT NULL,
    username        TEXT,
    first_joined_at TEXT,
    last_joined_at  TEXT,
    last_left_at    TEXT,
    join_count      INTEGER NOT NULL DEFAULT 0,
    message_count   INTEGER NOT NULL DEFAULT 0,
    last_seen_at    TEXT,
    currently_in    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (guild_id, user_id)
);
CREATE INDEX IF NOT EXISTS idx_members_guild ON members(guild_id);

CREATE TABLE IF NOT EXISTS messages (
    message_id  INTEGER NOT NULL PRIMARY KEY,
    guild_id    INTEGER NOT NULL,
    channel_id  INTEGER NOT NULL,
    author_id   INTEGER NOT NULL,
    content     TEXT,
    attachments TEXT,
    created_at  TEXT NOT NULL,
    deleted_at  TEXT,
    edited_at   TEXT
);
CREATE INDEX IF NOT EXISTS idx_messages_author ON messages(author_id, created_at);
CREATE INDEX IF NOT EXISTS idx_messages_guild ON messages(guild_id, created_at);
CREATE INDEX IF NOT EXISTS idx_messages_created ON messages(created_at);

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

CREATE TABLE IF NOT EXISTS mutes (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id     INTEGER NOT NULL,
    user_id      INTEGER NOT NULL,
    moderator_id INTEGER,
    until        TEXT,
    reason       TEXT,
    created_at   TEXT NOT NULL
);

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

CREATE TABLE IF NOT EXISTS users (
    user_id     INTEGER NOT NULL PRIMARY KEY,
    username    TEXT,
    global_name TEXT,
    avatar      TEXT,
    updated_at  TEXT
);
