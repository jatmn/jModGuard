-- userinfo is a shipped built-in added after migration 0021.
-- A guild or global row with that name cannot keep stored access or definition.
-- Custom commands under other names are left alone.
-- A second run matches no rows.
DELETE FROM commands
WHERE lower(name) = 'userinfo';
