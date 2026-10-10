-- One active ban per guild and user. Gateway echoes used to insert a second
-- active row next to the bot's row. Keep the active row that already has an
-- actor or a real reason, otherwise the earliest id. Inactive history stays,
-- so an unban followed by a later ban is still two rows.
DELETE FROM bans
WHERE active = 1
  AND EXISTS (
    SELECT 1 FROM bans keeper
    WHERE keeper.guild_id = bans.guild_id
      AND keeper.user_id = bans.user_id
      AND keeper.active = 1
      AND keeper.id <> bans.id
      AND (
        (CASE WHEN keeper.moderator_id IS NOT NULL THEN 2 ELSE 0 END
          + CASE WHEN keeper.reason IS NOT NULL AND keeper.reason <> 'Reason unavailable' THEN 1 ELSE 0 END)
        > (CASE WHEN bans.moderator_id IS NOT NULL THEN 2 ELSE 0 END
          + CASE WHEN bans.reason IS NOT NULL AND bans.reason <> 'Reason unavailable' THEN 1 ELSE 0 END)
        OR (
          (CASE WHEN keeper.moderator_id IS NOT NULL THEN 2 ELSE 0 END
            + CASE WHEN keeper.reason IS NOT NULL AND keeper.reason <> 'Reason unavailable' THEN 1 ELSE 0 END)
          = (CASE WHEN bans.moderator_id IS NOT NULL THEN 2 ELSE 0 END
            + CASE WHEN bans.reason IS NOT NULL AND bans.reason <> 'Reason unavailable' THEN 1 ELSE 0 END)
          AND keeper.id < bans.id
        )
      )
  );

CREATE UNIQUE INDEX idx_bans_one_active ON bans(guild_id, user_id) WHERE active = 1;

-- The bot attempt that owns a gateway echo. A later command fills that row
-- only when this id is the same attempt. Null is not an attempt. The keeper
-- score above does not use this column.
ALTER TABLE bans ADD COLUMN operation_id TEXT;
