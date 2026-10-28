-- Built-in commands take access controls from the shipped definition.
-- Guild and global rows whose name is a shipped primary or reserved alias
-- cannot keep enabled, permission, roles, channels, or cooldown_secs.
-- After those fields are dropped, the row holds nothing a guild may store
-- (definition and aliases are shipped, and writes of these names are rejected),
-- so the row is removed. Custom commands under other names are left alone,
-- including ones that merely mention a reserved alias in their JSON.
-- A second run matches no rows.
DELETE FROM commands
WHERE lower(name) IN (
    'about',
    'b',
    'ban',
    'banlist',
    'bans',
    'clear',
    'commands',
    'echo',
    'h',
    'help',
    'k',
    'kick',
    'logchannel',
    'mute',
    'p',
    'prefix',
    'purge',
    'rule',
    'say',
    'setname',
    'slowmode',
    'stats',
    'status',
    't',
    'template',
    'testregex',
    'timeout',
    'ub',
    'unban',
    'voice',
    'w',
    'warn',
    'warnclear',
    'warns',
    'yeet'
);
