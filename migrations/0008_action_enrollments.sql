-- Bind new work explicitly; never assign an old request to an observed membership.
ALTER TABLE bot_actions ADD COLUMN enrollment_id TEXT;
ALTER TABLE bot_actions ADD COLUMN joined_at_ns INTEGER;
DROP INDEX bot_actions_active_resource;
CREATE UNIQUE INDEX bot_actions_active_resource ON bot_actions(guild_id,enrollment_id,resource)
    WHERE state IN ('pending','claimed','running','uncertain');
CREATE INDEX bot_actions_enrollment ON bot_actions(guild_id,enrollment_id,created_at DESC,id DESC);
CREATE INDEX bot_actions_guild_state ON bot_actions(guild_id,state);
UPDATE bot_actions SET state='cancelled',outcome='unbound_membership',finished_at=unixepoch()
    WHERE state IN ('pending','claimed');
UPDATE bot_actions SET state='uncertain',outcome='unbound_membership'
    WHERE state='running';
