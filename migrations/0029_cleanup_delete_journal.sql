CREATE TABLE cleanup_delete_journal (
    batch TEXT NOT NULL,
    message_id INTEGER NOT NULL,
    owner TEXT NOT NULL,
    guild_id INTEGER NOT NULL,
    enrollment_id TEXT NOT NULL,
    channel_id INTEGER NOT NULL,
    user_id INTEGER NOT NULL,
    rule_id TEXT NOT NULL,
    rule_name TEXT NOT NULL,
    observed_at_ms INTEGER NOT NULL,
    purge INTEGER NOT NULL,
    dispatching INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (batch, message_id)
);
CREATE INDEX cleanup_delete_journal_guild ON cleanup_delete_journal(guild_id);
