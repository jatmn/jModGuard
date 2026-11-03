ALTER TABLE bans ADD COLUMN expires_at INTEGER;
ALTER TABLE bans ADD COLUMN expiry_enrollment_id TEXT;
CREATE INDEX idx_bans_expiry ON bans(expires_at) WHERE active=1 AND expires_at IS NOT NULL;
