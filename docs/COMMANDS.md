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
| `!userinfo [user]` | Moderation | mod | Cached profile, warnings, and recent cases (defaults to you) |
| `!purge [count]` | Moderation | mod | Delete recent messages (max 100; `!clear`, `!p`) |
| `!say [#channel] <text>` | Moderation | mod | Post a message as the bot (`!echo`) |
| `!slowmode <seconds>` | Moderation | mod | Set slowmode on this channel |
| `!banlist [limit]` | Moderation | mod | Recent bans tracked by the bot (`!bans`) |
| `!setname <name>` | Admin | admin | Queue the bot's nickname for application in this server |
| `!prefix <prefix>` | Admin | admin | Change this server's command prefix |
| `!template <list\|get\|set\|reset> [event] [text]` | Admin | admin | Edit log event templates |
| `!rule <list\|toggle\|dryrun> [rule] [on\|off]` | Admin | admin | Toggle rules or set dry-run mode (including exact-image and repeated-text rules) |
| `!logchannel <list\|set\|unset> [event] [#channel]` | Admin | admin | Route events to channels |
| `!testregex <pattern> <sample>` | Admin | admin | Test a regex against raw + normalized text |
| `!help [command]` | Utility | everyone | List commands (`!commands`, `!h`) |
| `!about` | Utility | everyone | Bot info |
| `!stats` | Utility | admin | Uptime, this server’s retained moderation counts, release state, and this bot container’s CPU and memory (alias: `!status`) |

Slash equivalents exist for the mod/admin commands (`/ban`, `/kick`, `/timeout`,
`/warn`, `/warns`, `/warnclear`, `/userinfo`, `/purge`, `/say`, `/slowmode`, `/unban`,
`/setname`, `/prefix`, `/template`, `/rule`, `/testregex`, `/help`, `/about`,
`/stats`).

For `/rule`, set `name` to the rule ID, `action` to `dryrun`, and `value` to
`on` or `off`. `off` switches exact-image and repeated-text rules to normal
delete-and-ban mode.

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
behavior is a **list of steps** executed in order. Plans with target-capable
steps accept an optional first user argument (or slash `user` option). When
supplied, it is the target; targeted plans require the command's permission to
be at least `mod`. Without a target, self-service steps use the caller.
`timeout`, `kick`, and `ban` require a target.

| Step | Behavior |
|---|---|
| `reply` | send a message in the invoking channel (templates supported) |
| `dm` | DM the target user (or the caller when omitted) |
| `log` | emit a log event to its configured channel |
| `add_role` / `remove_role` | grant/revoke a role on the target (or caller when omitted) |
| `timeout` | timeout the required target |
| `kick` / `ban` | kick/ban the required target (gated by the command's permission) |
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
{seconds} {duration} {response}`. Leave it empty and the behavior's built-in reply (if
any) is used. When `!say` posts into the invoking channel the reply is
suppressed as redundant — the posted text is already visible there — so a
prefix invocation sends nothing and the deferred slash answer is withdrawn.
A post into another channel still confirms in the invoking channel.

For timeout and slowmode, `{duration}` is the compact interval (for example
`1h 30m` or `1m 30s`). `{minutes}` and `{seconds}` retain their raw numeric
values for custom responses. The bot renders the exact shipped timeout and
slowmode response wording compactly at reply time; saved definitions remain
compatible with older bots, and other custom wording is preserved.

## Alias notes

Prefix aliases match exactly. Slash command names must be
`[a-z0-9_-]` — the slash sync sanitizes names automatically.

Stats uses bold labels and inline-code values. Uptime, moderation records, warns,
and bans have a leading emote. Uptime and release ages use compact months, weeks,
days, hours, minutes, and seconds (`13m 38s`; intermediate zeros stay, as in
`1d 0h 0m 38s`), with fixed 30-day months and 7-day weeks,
rounded down to whole seconds; a newly started process shows `0s`. Release
availability is an icon: ✅ up to date, ⬆️ update available, ⚠️ source matches
but the image digest is unverified, ❓ unknown. A component with no cached
release row is `❓` and has no SHA or age. Container use is this bot
process's own cgroup. Memory is real program use (`memory.current` minus
`inactive_file`) shown against `memory.max` (`max` means no limit). CPU is the
latest background sample. `!status` does not wait for one; until a sample
exists that row is `unavailable`. When `JMODGUARD_READ_TARGET` points at the
API loopback read listener (for example `127.0.0.1:8083`) and
`JMODGUARD_CONTROL_SECRET` matches, the bot fetches
the API container sample in parallel with the rest of the reply (short
timeout). The label names whichever API copy answered; it is not an aggregate.
A missing or slow reading is `unavailable`, never `0`.

Prefix `!status`
uses the same stats command, permissions and response; the slash name remains
`/stats`. On upgrade, untouched saved stats definitions gain the alias once.
Customized definitions remain unchanged; add `status` in the Commands editor if
needed. If any saved command already uses that name or alias, automatic alias
upgrade is skipped to preserve its routing.
