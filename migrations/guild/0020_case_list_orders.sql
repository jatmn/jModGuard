-- Keep list keys in sync with the original case facts, including outcome settlement.
-- Snowflakes are serialized as strings. Sort numerically and put absent actors at zero.
ALTER TABLE guild_cases ADD COLUMN list_date INTEGER AS (json_extract(payload, '$.occurred_at_ms')) VIRTUAL;
ALTER TABLE guild_cases ADD COLUMN list_action TEXT AS (json_extract(payload, '$.action')) VIRTUAL;
ALTER TABLE guild_cases ADD COLUMN list_outcome TEXT AS (json_extract(payload, '$.outcome')) VIRTUAL;
ALTER TABLE guild_cases ADD COLUMN list_subject INTEGER AS (CAST(json_extract(payload, '$.subject_id') AS INTEGER)) VIRTUAL;
ALTER TABLE guild_cases ADD COLUMN list_actor INTEGER AS (COALESCE(CAST(json_extract(payload, '$.actor_id') AS INTEGER), 0)) VIRTUAL;

CREATE INDEX guild_cases_list_date ON guild_cases(guild_id,enrollment_id,list_date,case_id) WHERE deleted=0;
CREATE INDEX guild_cases_list_action ON guild_cases(guild_id,enrollment_id,list_action,case_id) WHERE deleted=0;
CREATE INDEX guild_cases_list_outcome ON guild_cases(guild_id,enrollment_id,list_outcome,case_id) WHERE deleted=0;
CREATE INDEX guild_cases_list_subject ON guild_cases(guild_id,enrollment_id,list_subject,case_id) WHERE deleted=0;
CREATE INDEX guild_cases_list_actor ON guild_cases(guild_id,enrollment_id,list_actor,case_id) WHERE deleted=0;
