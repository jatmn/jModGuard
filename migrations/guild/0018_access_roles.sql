-- Compatible upgrade from guild schema 18 to 19. Role ID -> Web UI access
-- level mappings for this membership. Existing rows keep no mappings.
ALTER TABLE guild_tenant_settings ADD COLUMN access_roles TEXT NOT NULL DEFAULT '[]';
