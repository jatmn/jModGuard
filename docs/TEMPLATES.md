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
| `{before}` / `{after}` | last retained text before an observed edit, when available; accepted after text or supplied update text if retention fails. Missing and empty are distinct |
| `{message_id}` / `{message_url}` | single-message ID and Discord reference (edit/delete reports) |
| `{attachments}` | retained attachment metadata when available, or supplied update details / an explicitly unavailable value |
| `{coverage}` | last-observed continuity or a gap / unavailable reason for edit and single-delete reports |
| `{retained}` / `{unavailable}` / `{filtered}` / `{details}` | bulk-delete counts, excluded authors and transcript availability |
| `{count}` | bulk delete count / purge count / warn count |
| `{duration}` | compact timeout length / slowmode interval / time in server |
| `{account_created}` | account creation timestamp |
| `{account_age}` | compact account age |
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
- **message_edit** — one Discord-identified edit (`edited_timestamp`), last observed before and updated text with coverage label. Embed/GIF unfurls, chat replies and the bot's own log posts do not create edit reports.
- **message_delete** — message ID and last observed author, text and attachment metadata when available
- **message_bulk_delete** — unique count, retained/known-missing/storage-unknown/filtered counts, omitted transcript entries, and a transient bounded transcript attachment; a definite attachment-permission failure uses plain-message chunks with an exact omitted count
- **automod** — shield: rule, action, reason, evidence, channel
- **mod_action** — judge: action, user, moderator, reason
- **timeout_add** / **timeout_remove** — timer events
- **plan_executed** — custom command runs
- **system** — general notices, including one start notice per process and a
  graceful-shutdown notice with compact uptime

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
The retained-message template upgrade likewise replaces only byte-for-byte shipped
edit/delete/bulk defaults once. Custom wording stays intact.
The bulk status upgrade updates only the exact earlier retained-bulk default to
show known missing, storage unknown and omitted entries; custom text stays intact.

All 14 built-in log templates use bold headings and labels, inline-code IDs,
counts/durations/dates, and an absolute event time. Mentions and links remain
outside code spans. Dates display as `Sep 22, 2026 • 18:02:22 UTC`: whole seconds
and the UTC timezone remain; fractional seconds are omitted. Edit reports use
Discord's supplied edit timestamp. Events without a supplied timestamp use report
time. Date formatting also applies to the standard `{time}`, `{account_created}`
and `{joined}` variables in custom templates; other content is not rewritten.

Durations use the same compact format as Stats: `3m 36s`, or
`2mo 1w 3d 4h 5m 6s` for longer spans. Units are `mo`, `w`, `d`, `h`, `m`,
and `s`, using fixed 30-day months and 7-day weeks. Leading and trailing zero
units are omitted; intermediate zeros remain (`1d 0h 0m 38s`). Zero is `0s`.
Missing leave context says `Unavailable`.
The timeout and slowmode command replies also expose `{duration}`; raw
`{minutes}` and `{seconds}` remain available for custom command responses.
The bot renders the exact shipped timeout and slowmode response wording in
compact form without rewriting saved command definitions, so older processes
can still render those defaults during rollout or rollback. Other custom response
wording is preserved; use `{duration}` to opt into compact formatting.
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
