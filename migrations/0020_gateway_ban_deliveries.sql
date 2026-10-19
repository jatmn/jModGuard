-- Received ban lifecycle facts. Delivery identity survives ambiguous commits;
-- pending work is durable before its in-memory payload is released.
CREATE TABLE gateway_ban_deliveries (
    id TEXT PRIMARY KEY NOT NULL,
    guild_id INTEGER NOT NULL,
    enrollment_id TEXT NOT NULL,
    user_id INTEGER NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('ban', 'unban', 'audit')),
    observed_at_ms INTEGER NOT NULL,
    payload TEXT NOT NULL,
    applied INTEGER NOT NULL DEFAULT 0 CHECK (applied IN (0, 1)),
    case_saved INTEGER NOT NULL CHECK (case_saved IN (0, 1)),
    ban_result TEXT
);
CREATE INDEX idx_gateway_ban_deliveries_pending
    ON gateway_ban_deliveries(applied, case_saved);
CREATE INDEX idx_gateway_ban_deliveries_subject
    ON gateway_ban_deliveries(guild_id, user_id, kind, observed_at_ms);
