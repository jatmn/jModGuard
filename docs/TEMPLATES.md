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
| `{actor}` | moderator who performed the action (`name (<@id>)`, ID mention if name unavailable); explicitly unavailable if ban/unban audit lookup fails |
| `{actor_id}` | actor's numeric ID; unavailable for unattributed ban/unban reports |
| `{reason}` | reason (rendered action reason for automod) |
| `{rule}` | automod rule name |
| `{action}` | automod action performed |
| `{strikes}` | strike count at the time of the action |
| `{content}` | message content / evidence / plan detail |
| `{before}` / `{after}` | `before` is not retained; `after` is supplied update text or explicitly unavailable |
| `{message_id}` / `{message_url}` | single-message ID and Discord reference (edit/delete reports) |
| `{attachments}` | supplied update attachment count; for a single deletion, bounded URLs from a recent observation, or explicitly unavailable |
| `{count}` | bulk delete count / purge count / warn count |
| `{duration}` | timeout length / time in server |
| `{account_created}` | account creation timestamp |
| `{account_age}` | readable account age |
| `{joined}` | supplied/cached join timestamp in join/leave reports (readable UTC date), or unavailable |
| `{time}` | event timestamp (readable UTC date and time, including seconds) |
| `{command}` | command name (command log events) |
| `{response}` | behavior's inline reply (command response templates) |

Unknown variables are left untouched so template mistakes are visible.

## Default templates (all editable)

- **member_join** — joined the server, account created/age
- **member_leave** — left, elapsed time and join date from cached member or recently received message member details; no historical join count
- **member_ban** / **member_unban** — hammer/unlock with recent matching audit actor + reason (requires View Audit Log); explicit unavailable fallback
- **member_kick** — boot with actor + reason
- **message_edit** — one Discord-identified edit (`edited_timestamp`); supplied current text, attachment count and message reference. Not embed/GIF unfurls, chat replies, or the bot's own log posts.
- **message_delete** — message ID; recent observed author, text and attachment URLs when available
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
message_delete→  🗑 Message `{message_id}` deleted in {channel}; text: {content}; attachments: {attachments}.
```

Discord reports are bounded to 2000 UTF-16 units, including truncation markers.
Mentions and automatic link previews are suppressed. Individual supplied text fields
may be clipped earlier. Unknown variables remain visible; availability depends on
the event. See [logging/data behavior](LOGGING-DATA.md).

## Existing saved templates

The first bot start with the readable-log upgrade replaces only exact matches
to the previous shipped templates, in both global and guild scopes. Custom wording
is preserved. The upgrade is recorded once, so later operator edits are not undone
on restart. Configuration versioning makes the upgraded values visible to the bot
and WebUI.

All 14 built-in log templates use bold headings and labels, inline-code IDs,
counts/durations/dates, and an absolute event time. Mentions and links remain
outside code spans. Dates display as `Sep 22, 2026 • 18:02:22 UTC`: whole seconds
and the UTC timezone remain; fractional seconds are omitted. Edit reports use
Discord's supplied edit timestamp. Events without a supplied timestamp use report
time. Date formatting also applies to the standard `{time}`, `{account_created}`
and `{joined}` variables in custom templates; other content is not rewritten.

Durations list nonzero months, weeks, days, hours, minutes and seconds, using
30-day months and 7-day weeks. Missing leave context says `Unavailable`.
The leave template retains the supplied display name, user ID, time in server,
join date (when known), and departure time; it does not depend on a resolved
mention after departure.

To change a preserved custom template, edit it in **Templates** or with
`!template`. Resetting a guild override inherits the saved global value; the bot
owner can edit the global template separately. Existing historical Discord posts
are not rewritten, and no missing history is recovered.
The attachment detail upgrade replaces only exact copies of the previous shipped
single-delete template. Custom templates remain unchanged; add `{attachments}`
to one if you want its deletion reports to show observed URLs.
