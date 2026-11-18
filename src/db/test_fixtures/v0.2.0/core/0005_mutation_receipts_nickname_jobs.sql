CREATE TABLE mutation_receipts (
    receipt_id TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL
);
CREATE INDEX idx_mutation_receipts_created_at ON mutation_receipts (created_at);

ALTER TABLE nickname_reconcile ADD COLUMN state TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE nickname_reconcile ADD COLUMN attempt_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE nickname_reconcile ADD COLUMN next_attempt_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE nickname_reconcile ADD COLUMN last_error_code TEXT;
ALTER TABLE nickname_reconcile ADD COLUMN last_error TEXT;
ALTER TABLE nickname_reconcile ADD COLUMN last_error_at INTEGER;

CREATE INDEX idx_nickname_reconcile_due
    ON nickname_reconcile (state, next_attempt_at, guild_id);

-- Before the durable outbox existed, guild_create applied an existing
-- configured nickname directly. Preserve that upgrade behavior by scheduling
-- one initial reconciliation for every legacy guild-local nickname. Global
-- nicknames are intentionally unsupported, and malformed settings remain the
-- responsibility of the startup settings healer.
INSERT OR IGNORE INTO nickname_reconcile
    (guild_id, generation, state, attempt_count, next_attempt_at,
     last_error_code, last_error, last_error_at)
SELECT guild_id, 1, 'pending', 0, 0, NULL, NULL, NULL
FROM guild_settings
WHERE guild_id != 0
  AND COALESCE(
        json_extract(
            CASE WHEN json_valid(settings) THEN settings ELSE '{}' END,
            '$.bot_name'
        ),
        ''
      ) != '';
