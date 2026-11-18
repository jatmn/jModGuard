-- Keep the latest reason edit alongside the original so the ordered page can
-- use an index. The edit and its audit event are written in one transaction.
ALTER TABLE bans ADD COLUMN list_edited_reason TEXT;
ALTER TABLE bans ADD COLUMN list_reason_edited_by INTEGER;
ALTER TABLE bans ADD COLUMN list_reason_edited_at TEXT;

WITH latest AS (
    SELECT MAX(id) AS id
      FROM events
     WHERE event_type = 'ban_reason_edit' AND json_valid(data)
       AND json_type(data, '$.reason') = 'text'
     GROUP BY guild_id, CAST(json_extract(data, '$.ban_id') AS INTEGER)
)
UPDATE bans
   SET list_edited_reason = json_extract(events.data, '$.reason'),
       list_reason_edited_by = events.actor_id,
       list_reason_edited_at = events.created_at
  FROM latest JOIN events ON events.id = latest.id
 WHERE bans.guild_id = events.guild_id
   AND bans.id = CAST(json_extract(events.data, '$.ban_id') AS INTEGER);

ALTER TABLE bans ADD COLUMN list_subject TEXT AS
    (LOWER(COALESCE(NULLIF(user_display_name, ''), user_name, ''))) VIRTUAL;
ALTER TABLE bans ADD COLUMN list_moderator TEXT AS
    (LOWER(COALESCE(NULLIF(moderator_name, ''), CAST(moderator_id AS TEXT), ''))) VIRTUAL;
ALTER TABLE bans ADD COLUMN list_reason TEXT AS
    (LOWER(COALESCE(list_edited_reason, reason, ''))) VIRTUAL;
-- A legacy "now" value cannot be evaluated in an index. It has no saved time.
ALTER TABLE bans ADD COLUMN list_date INTEGER AS
    (COALESCE(CAST(ROUND((julianday(NULLIF(created_at COLLATE NOCASE, 'now')) - 2440587.5)
                    * 86400000) AS INTEGER), 0)) VIRTUAL;

CREATE INDEX idx_bans_list_subject ON bans(guild_id, list_subject, id);
CREATE INDEX idx_bans_list_moderator ON bans(guild_id, list_moderator, id);
CREATE INDEX idx_bans_list_reason ON bans(guild_id, list_reason, id);
CREATE INDEX idx_bans_list_date ON bans(guild_id, list_date, id);
CREATE INDEX idx_bans_list_status ON bans(guild_id, active, id);
CREATE INDEX idx_bans_list_actor ON bans(guild_id, moderator_id);
