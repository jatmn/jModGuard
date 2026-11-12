-- Compatible upgrade from guild schema 17 to 18. A coverage gap that the bot
-- recorded as a short restart may be cleared by completed audit recovery.
-- Existing gaps keep 0 and still need an explicit Reconcile.
ALTER TABLE guild_member_seed ADD COLUMN gap_auto_clear INTEGER NOT NULL DEFAULT 0 CHECK(gap_auto_clear IN (0,1));
