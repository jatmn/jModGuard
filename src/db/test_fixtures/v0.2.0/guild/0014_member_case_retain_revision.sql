-- Case retention updates directory ban history but must not invalidate in-flight
-- roster import pages through guild_member_seed.directory_revision.
DROP TRIGGER guild_member_case_retain;
CREATE TRIGGER guild_member_case_retain AFTER INSERT ON guild_cases
WHEN NEW.deleted=0 AND json_valid(NEW.payload)
 AND json_extract(NEW.payload,'$.outcome')='succeeded'
 AND (json_extract(NEW.payload,'$.action')='ban'
      OR (json_extract(NEW.payload,'$.action')='unban'
          AND (NEW.source_key LIKE 'discord:%'
               OR json_type(NEW.payload,'$.source.gateway_observation') IS NOT NULL)))
BEGIN
 INSERT INTO guild_member_directory(guild_id,enrollment_id,user_id,ever_banned,observed_at_ms)
 VALUES(NEW.guild_id,NEW.enrollment_id,CAST(json_extract(NEW.payload,'$.subject_id') AS INTEGER),1,CAST(json_extract(NEW.payload,'$.occurred_at_ms') AS INTEGER))
 ON CONFLICT(guild_id,enrollment_id,user_id) DO UPDATE SET ever_banned=1,cleanup_after_ms=NULL,revision=revision+1;
END;
