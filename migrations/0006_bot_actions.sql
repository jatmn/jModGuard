-- Inactive action-queue foundation; no worker or HTTP admission is enabled.
CREATE TABLE bot_actions (
    id TEXT PRIMARY KEY,
    guild_id INTEGER NOT NULL CHECK(guild_id > 0),
    actor_id INTEGER NOT NULL CHECK(actor_id > 0),
    request_key TEXT NOT NULL,
    request_created_at INTEGER NOT NULL,
    resource TEXT NOT NULL,
    payload TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('pending','claimed','running','succeeded','failed','uncertain','cancelled','expired')),
    outcome TEXT,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    claim TEXT,
    claimed_at INTEGER,
    finished_at INTEGER,
    UNIQUE(guild_id, request_key)
);
CREATE UNIQUE INDEX bot_actions_active_resource ON bot_actions(guild_id,resource)
    WHERE state IN ('pending','claimed','running','uncertain');
CREATE INDEX bot_actions_pending ON bot_actions(state,created_at,id);
CREATE INDEX bot_actions_guild_recent ON bot_actions(guild_id,created_at DESC,id);

CREATE INDEX bot_actions_finished ON bot_actions(max(finished_at,request_created_at),id)
    WHERE state IN ('succeeded','failed','cancelled','expired') AND request_created_at>0;
INSERT INTO meta(key,value) VALUES('action_request_floor','0');
