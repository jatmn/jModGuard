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
| `{before}` / `{after}` | `before` is not retained; `after` is supplied update text or explicitly unavailable |
| `{message_id}` / `{message_url}` | single-message ID and Discord reference (edit/delete reports) |
| `{attachments}` | supplied update attachment count, or explicitly unavailable |
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
- **member_leave** — left, cached join timestamp when available; no historical join count
- **member_ban** / **member_unban** — hammer/unlock with actor + reason
- **member_kick** — boot with actor + reason
- **message_edit** — one Discord-identified edit (`edited_timestamp`); supplied current text, attachment count and message reference. Not embed/GIF unfurls, chat replies, or the bot's own log posts.
- **message_delete** — message ID; author/content unavailable
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
message_delete→  🗑 Message `{message_id}` deleted in {channel}; author/content unavailable.
```

Discord reports are bounded to 2000 UTF-16 units, including truncation markers.
Mentions and automatic link previews are suppressed. Individual supplied text fields
may be clipped earlier. Unknown variables remain visible; availability depends on
the event. See [logging/data behavior](LOGGING-DATA.md).

## Existing saved templates

Upgrades preserve all saved guild/global templates, including previously seeded defaults.
The old edit/delete wording therefore remains until an operator edits it. To adopt the
new basic reports, open **Templates**, select **This server**, and save these values:

```text
message_edit: Message updated in {channel} — {message_url} — Author: {mention} — Current text: {after} — Attachments: {attachments}
message_delete: Message `{message_id}` deleted in {channel}; author unavailable; content not retained.
```

The bot owner can instead save them with **Global** selected, then clear any unwanted
guild overrides using **Reset to global**. Resetting a guild alone still inherits the
saved global value; it does not replace that value with the compiled default. Existing
custom wording can keep its useful variables and add the new fields. No bootstrap or
database migration is needed. This does not recover older/deleted text.
