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
