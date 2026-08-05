# Commands

Every command is **data**: it lives in the database (per-server, with a global
fallback), can be renamed, reworded, re-permissioned, disabled, or given a
cooldown — all from the WebUI **Commands** tab. Enabled commands are also
registered as Discord **slash commands** (same logic, different invocation).

Prefix commands: `<prefix><name> <args>` (default prefix `!`).

## Built-in commands

| Command | Category | Permission | Description |
|---|---|---|---|
| `!ban <user> [reason]` | Moderation | mod | Ban a user (aliases: `!yeet`, `!b`) |
| `!unban <user> [reason]` | Moderation | mod | Unban a user (`!ub`) |
| `!kick <user> [reason]` | Moderation | mod | Kick a user (`!k`) |
| `!timeout <user> <minutes> [reason]` | Moderation | mod | Timeout a user; `0` removes it (`!mute`, `!t`) |
| `!warn <user> <reason>` | Moderation | mod | Warn a user (`!w`) |
| `!warns <user>` | Moderation | mod | List a user's warnings |
| `!warnclear <user>` | Moderation | mod | Clear a user's warnings |
| `!purge [count]` | Moderation | mod | Delete recent messages (max 100; `!clear`, `!p`) |
| `!say [#channel] <text>` | Moderation | mod | Post a message as the bot (`!echo`) |
| `!slowmode <seconds>` | Moderation | mod | Set slowmode on this channel |
| `!banlist [limit]` | Moderation | mod | Recent bans tracked by the bot (`!bans`) |
| `!setname <name>` | Admin | admin | Set the bot's nickname in this server |
| `!prefix <prefix>` | Admin | admin | Change this server's command prefix |
| `!template <list\|get\|set\|reset> [event] [text]` | Admin | admin | Edit log event templates |
| `!rule <list\|toggle\|dryrun> [rule]` | Admin | admin | Toggle rules / dry-run mode |
| `!logchannel <list\|set\|unset> [event] [#channel]` | Admin | admin | Route events to channels |
| `!testregex <pattern> <sample>` | Admin | admin | Test a regex against raw + normalized text |
| `!help [command]` | Utility | everyone | List commands (`!commands`, `!h`) |
| `!about` | Utility | everyone | Bot info |
| `!stats` | Utility | everyone | Uptime + database statistics |

Slash equivalents exist for the mod/admin commands (`/ban`, `/kick`, `/timeout`,
`/warn`, `/warns`, `/warnclear`, `/purge`, `/say`, `/slowmode`, `/unban`,
`/setname`, `/prefix`, `/template`, `/rule`, `/testregex`, `/help`, `/about`,
`/stats`).

## Permission levels

Checked before a command runs; the *caller's* level decides access:

| Level | Requirement |
|---|---|
| `everyone` | any member |
| `mod` | has Kick/Ban Members, Manage Messages, or Moderate Members |
| `admin` | Administrator or Manage Server |
| `owner` | the `bot.owner_id` in `config.toml` |

Extra per-command restrictions: `roles` (must hold one of these role IDs) and
`channels` (command only works in these channel IDs).

## Custom commands (action plans)

The WebUI **Commands → New custom command** editor builds a command whose
behavior is a **list of steps** executed in order:

| Step | Behavior |
|---|---|
| `reply` | send a message in the invoking channel (templates supported) |
| `dm` | DM the target user (first argument, or the caller) |
| `log` | emit a log event to its configured channel |
| `add_role` / `remove_role` | grant/revoke a role on the caller |
| `timeout` | timeout the caller |
| `kick` / `ban` | kick/ban the caller (gated by the command's permission) |
| `delete_command_message` | delete the message that invoked the command |
| `react` | add an emoji reaction to the invoking message |
| `wait` | sleep N seconds before the next step |

Every step is audited; a failing step is logged and execution continues.

Example: a `!welcome` plan: `reply "Hi {user}, welcome to {guild}!"` →
`react 👋` → `log (plan_executed) "`{actor}` ran `{command}`"`.

## Response templates

Each command's `response` field is a template rendered after success, e.g.
`Banned {target} | {reason}`. Variables: `{user} {actor} {actor_id} {guild}
{channel} {target} {target_id} {reason} {minutes} {count} {name} {prefix}
{seconds} {response}`. Leave it empty and the behavior's built-in reply (if
any) is used.

## Alias notes

Prefix aliases match exactly. Slash command names must be
`[a-z0-9_-]` — the slash sync sanitizes names automatically.
