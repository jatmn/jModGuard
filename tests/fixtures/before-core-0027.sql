-- Historical schema fixtures rewind the current test database before migrating.
DROP INDEX idx_bans_list_subject;
DROP INDEX idx_bans_list_moderator;
DROP INDEX idx_bans_list_reason;
DROP INDEX idx_bans_list_date;
DROP INDEX idx_bans_list_status;
DROP INDEX idx_bans_list_actor;
ALTER TABLE bans DROP COLUMN list_subject;
ALTER TABLE bans DROP COLUMN list_moderator;
ALTER TABLE bans DROP COLUMN list_reason;
ALTER TABLE bans DROP COLUMN list_date;
ALTER TABLE bans DROP COLUMN list_edited_reason;
ALTER TABLE bans DROP COLUMN list_reason_edited_by;
ALTER TABLE bans DROP COLUMN list_reason_edited_at;
