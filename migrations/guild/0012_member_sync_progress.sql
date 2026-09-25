-- Preserve exact per-phase committed counts and recent page cadence. Existing
-- v10 aggregate counters cannot reconstruct old ban/audit phase counts.
ALTER TABLE guild_member_sync ADD COLUMN member_processed INTEGER;
ALTER TABLE guild_member_sync ADD COLUMN ban_processed INTEGER;
ALTER TABLE guild_member_sync ADD COLUMN verify_processed INTEGER;
ALTER TABLE guild_member_sync ADD COLUMN audit_processed INTEGER;
ALTER TABLE guild_member_sync ADD COLUMN member_page_interval_ms INTEGER;
