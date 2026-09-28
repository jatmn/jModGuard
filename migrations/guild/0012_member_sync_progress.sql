-- Preserve exact per-phase committed counts and recent page cadence. This step
-- upgrades guild schema 11 to 12. The aggregate counters added in 0010 cannot
-- reconstruct old ban/audit phase counts.
ALTER TABLE guild_member_sync ADD COLUMN member_processed INTEGER;
ALTER TABLE guild_member_sync ADD COLUMN ban_processed INTEGER;
ALTER TABLE guild_member_sync ADD COLUMN verify_processed INTEGER;
ALTER TABLE guild_member_sync ADD COLUMN audit_processed INTEGER;
ALTER TABLE guild_member_sync ADD COLUMN member_page_interval_ms INTEGER;
