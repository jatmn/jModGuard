-- Durable installation-wide Discord directory request budget. The bot identity
-- owns one slot across guild stores and competing processes sharing this core.
CREATE TABLE member_sync_pacing (
    bot_id INTEGER PRIMARY KEY CHECK (bot_id > 0),
    request_after_ms INTEGER NOT NULL CHECK (request_after_ms >= 0),
    lease_owner TEXT,
    lease_until_ms INTEGER,
    generation INTEGER NOT NULL DEFAULT 0 CHECK (generation >= 0),
    CHECK ((lease_owner IS NULL) = (lease_until_ms IS NULL))
);
