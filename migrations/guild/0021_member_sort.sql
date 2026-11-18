-- Virtual keys match the values displayed in Members, with a numeric key for
-- status labels and 0 for an unknown join date (the existing default order).
-- Using columns lets SQLite seek to both values in a keyset boundary.
ALTER TABLE guild_member_directory ADD COLUMN name_sort_key TEXT AS (
    lower(coalesce(nullif(coalesce(nickname,global_name,username),''),nullif(username,''),CAST(user_id AS TEXT)))
) VIRTUAL;
ALTER TABLE guild_member_directory ADD COLUMN joined_sort_key INTEGER AS (coalesce(joined_at_ms,0)) VIRTUAL;
ALTER TABLE guild_member_directory ADD COLUMN status_sort_key INTEGER AS (
    CASE WHEN is_banned=1 THEN 1 WHEN is_current=1 THEN 0 WHEN ever_banned=1 THEN 3 ELSE 2 END
) VIRTUAL;

-- Partial indexes let each filtered list page through just its eligible rows.
-- The directory primary key supplies the all-status user ID order.
CREATE INDEX guild_member_sort_all_name
    ON guild_member_directory(guild_id,enrollment_id,name_sort_key,user_id);
CREATE INDEX guild_member_sort_all_joined
    ON guild_member_directory(guild_id,enrollment_id,joined_sort_key,user_id);
CREATE INDEX guild_member_sort_all_status
    ON guild_member_directory(guild_id,enrollment_id,status_sort_key,user_id);
CREATE INDEX guild_member_sort_current_name
    ON guild_member_directory(guild_id,enrollment_id,name_sort_key,user_id) WHERE is_current=1;
CREATE INDEX guild_member_sort_current_id
    ON guild_member_directory(guild_id,enrollment_id,user_id) WHERE is_current=1;
CREATE INDEX guild_member_sort_current_joined
    ON guild_member_directory(guild_id,enrollment_id,joined_sort_key,user_id) WHERE is_current=1;
CREATE INDEX guild_member_sort_current_status
    ON guild_member_directory(guild_id,enrollment_id,status_sort_key,user_id) WHERE is_current=1;
CREATE INDEX guild_member_sort_banned_name
    ON guild_member_directory(guild_id,enrollment_id,name_sort_key,user_id) WHERE is_banned=1;
CREATE INDEX guild_member_sort_banned_id
    ON guild_member_directory(guild_id,enrollment_id,user_id) WHERE is_banned=1;
CREATE INDEX guild_member_sort_banned_joined
    ON guild_member_directory(guild_id,enrollment_id,joined_sort_key,user_id) WHERE is_banned=1;
CREATE INDEX guild_member_sort_banned_status
    ON guild_member_directory(guild_id,enrollment_id,status_sort_key,user_id) WHERE is_banned=1;
CREATE INDEX guild_member_sort_previous_name
    ON guild_member_directory(guild_id,enrollment_id,name_sort_key,user_id) WHERE ever_banned=1 AND is_banned=0;
CREATE INDEX guild_member_sort_previous_id
    ON guild_member_directory(guild_id,enrollment_id,user_id) WHERE ever_banned=1 AND is_banned=0;
CREATE INDEX guild_member_sort_previous_joined
    ON guild_member_directory(guild_id,enrollment_id,joined_sort_key,user_id) WHERE ever_banned=1 AND is_banned=0;
CREATE INDEX guild_member_sort_previous_status
    ON guild_member_directory(guild_id,enrollment_id,status_sort_key,user_id) WHERE ever_banned=1 AND is_banned=0;
