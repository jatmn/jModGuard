# Templates

Every event that the bot logs is rendered from a **template** before being
posted to its log channel. Templates are per-server by default and fall back
to a global default; they are edited in the WebUI **Templates** tab (with live
preview) or via `!template`.

## Variable reference

| Variable | Meaning |
|---|---|
| `{user}` | target user's display name |
| `{username}` | target user's username |
| `{user_id}` | target user's numeric ID |
| `{mention}` | `<@id>` mention of the target |
| `{guild}` | server name |
| `{channel}` | `<#id>` mention |
| `{actor}` | moderator who performed the action (`name (<@id>)`) |
| `{actor_id}` | actor's numeric ID |
| `{reason}` | reason (rendered action reason for automod) |
| `{rule}` | automod rule name |
| `{action}` | automod action performed |
| `{strikes}` | strike count at the time of the action |
| `{content}` | message content / evidence / plan detail |
| `{before}` / `{after}` | message edit diff |
| `{count}` | bulk delete count / purge count / warn count |
| `{duration}` | timeout length / time in server |
| `{account_created}` | account creation timestamp |
| `{account_age}` | account age in days |
| `{joined}` | join timestamp |
| `{time}` | event timestamp (UTC RFC3339) |
| `{command}` | command name (command log events) |
| `{response}` | behavior's inline reply (command response templates) |

Unknown variables are left untouched so template mistakes are visible.

## Default templates (all editable)

- **member_join** — joined the server, account created/age
- **member_leave** — left, duration in server, join count
- **member_ban** / **member_unban** — hammer/unlock with actor + reason
- **member_kick** — boot with actor + reason
- **message_edit** — before/after content in channel
- **message_delete** — deleted content in channel
- **message_bulk_delete** — count in channel
- **automod** — shield: rule, action, reason, evidence, channel
- **mod_action** — judge: action, user, moderator, reason
- **timeout_add** / **timeout_remove** — timer events
- **plan_executed** — custom command runs
- **system** — general notices

## Log channel routing

Each event type routes to its own channel (multiple channels per server are
supported — one per event). Configure in the WebUI **Log Channels** tab or
with `!logchannel`:

```
!logchannel set automod #mod-alerts
!logchannel set mod_action #mod-log
!logchannel set member_join #welcome-log
!logchannel list
!logchannel unset automod        # clears this server's mapping
```

Log-channel mappings are always per-server: Discord channel IDs cannot safely
be shared as global defaults.

## Example customizations

```
member_join   →  📥 {user} joined! Say hi in #welcome ({mention})
automod       →  ⛔ {rule} fired on {mention} → {action} | {reason} | {content}
message_delete→  🗑 {mention}: "{content}" deleted in {channel}
```

Rendered length is limited to 1900 characters per event.
