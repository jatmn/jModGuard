-- Replace the virtual column and rebuild only the indexes that depend on it.
DROP INDEX guild_member_sort_all_name;
DROP INDEX guild_member_sort_current_name;
DROP INDEX guild_member_sort_banned_name;
DROP INDEX guild_member_sort_previous_name;

ALTER TABLE guild_member_directory DROP COLUMN name_sort_key;
ALTER TABLE guild_member_directory ADD COLUMN name_sort_key TEXT AS (
    lower(coalesce(nullif(nickname, ''), nullif(global_name, ''), nullif(username, ''), CAST(user_id AS TEXT)))
) VIRTUAL;

CREATE INDEX guild_member_sort_all_name
    ON guild_member_directory(guild_id, enrollment_id, name_sort_key, user_id);
CREATE INDEX guild_member_sort_current_name
    ON guild_member_directory(guild_id, enrollment_id, name_sort_key, user_id) WHERE is_current=1;
CREATE INDEX guild_member_sort_banned_name
    ON guild_member_directory(guild_id, enrollment_id, name_sort_key, user_id) WHERE is_banned=1;
CREATE INDEX guild_member_sort_previous_name
    ON guild_member_directory(guild_id, enrollment_id, name_sort_key, user_id) WHERE ever_banned=1 AND is_banned=0;
