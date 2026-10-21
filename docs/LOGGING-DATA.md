# Logging and data collection

The old general SQL message archive remains retired. A separate, rolling guild-store
snapshot keeps the latest observed text and attachment metadata for eligible human
messages in guilds with an edit, delete or bulk-delete log destination. It expires
at most 24 hours after the original message creation; edits never extend that time.
Capacity pressure can evict it earlier. It stores no attachment bytes. The bounded [member directory](MEMBERS.md) retains current
member identity and known join dates, plus people with confirmed ban history. It does
not maintain message activity counters or the retired general user archive.
Joining, leaving and talking still do not maintain a general activity archive.

## Basic reports

- Updates use supplied content/attachment fields and the last retained observation,
  when available. `before` is explicitly the last observed text; startup before
  collector registration, restart, overload and missing observations can leave a gap before
  the actual edit. A supplied author sets the display name; for ID-only filtering, a
  recent observation of that exact message can supply its author ID when the edit
  omits the author. An unknown author remains unavailable. An absent field is
  unknown, not empty. Discord sets `edited_timestamp` only for an actual edit;
  Gateway refreshes that omit it (embed, GIF/link unfurl, reply preview, pin,
  flags), including full objects that repeat the original content, a
  `message_reference`, or an empty attachment list, are not reports. The bot does
  not report Gateway updates of its own messages, so a log post cannot stack prior
  reports as current text. Each report is one observed edit. Reports say
  "updated"; they do not claim a verified complete revision diff. A bounded
  10-second fingerprint merges partial content/attachment updates
  that share an edit timestamp for the same message in one bot process. The
  stored snapshot does the same: a field inherited from an earlier observation
  is not treated as already supplied at the new timestamp, so a complementary
  partial can fill it. A different value for a field already supplied at that
  timestamp does not replace the first. A later distinct edit timestamp reports
  again.
- Each guild can save ignored Discord user IDs (including bot IDs) and source channel
  IDs separately for each event in Log Channels. Matching uses the IDs known to
  the report. If no user ID is known, only a source channel exclusion can apply.
  The filter applies before Discord delivery and does not alter moderation
  enforcement or retained moderation records.
- Single deletions take and remove the retained snapshot, reporting its last
  observed author, text and attachment metadata when available. Known empty text
  differs from an unsupplied field. A bounded recent process-only snippet may
  supply identity for filtering when no retained snapshot exists; its text and
  attachments never fill a retention miss. Attachment links can
  expire under Discord's signed CDN URL policy.
  Bulk deletions count each distinct message ID once across the entire event,
  distinguish retained, known missing, storage unknown and filtered entries,
  and attach a transient
  UTF-8 transcript capped at 256 KiB. The summary keeps the total and filtered
  counts without including ignored authors' content. A definite
  attachment-permission failure uses at most three bounded text chunks with the
  exact number of entries omitted from that fallback; an
  ambiguous timeout is never resent. The transcript is not archived.
- Join reports use the delivered user and the join timestamp in the Gateway member
  payload; they do not look up a saved directory join date. Leave reports use the
  saved directory stay when an authoritative departure decision is available,
  including an explicitly unknown join date. A departure needing verification waits
  for a bounded check; the verified leave uses the saved stay. Without an authoritative
  directory decision, leave reports fall back to the delivered/cached member join
  timestamp, then a join timestamp already supplied with a recent message.
  This last fallback retains only guild/user IDs
  and join timestamps in memory (at most 4096 entries, usable for one hour after
  observation), consumes them on leave and clears them on rejoin or guild reload/removal.
  Reports show the join date and elapsed months/weeks/days/hours/minutes/seconds when known. Missing join
  context is unavailable; no roster is fetched and no historical join count is fabricated.
- Ban/unban reports look up the recent Discord audit entry for the same target and
  action to display the moderator and actual reason. The bot needs **View Audit Log**.
  Lookup is best-effort: each attempt group makes at most three reads of the latest
  100 entries for that action under the report concurrency limit, using at most half
  the configured deadline (capped at three seconds). A gateway ban saves its
  row first, then tries; the report reuses a match or may try again if none was found.
  If the report finds the audit entry later, it updates only that saved ban row
  under the same membership and write-fence checks.
  Entries must be within ten seconds before to three seconds
  after the observed event; multiple matching entries are ambiguous. A missing,
  inaccessible, late or ambiguous entry produces explicit unavailable fields.
  A matching entry with no reason says "No reason provided". These display lookups
  do not create or change retained case-source associations and do not recover old logs.
  Matched audit IDs are consumed before actor filtering or delivery, so a rapid repeat
  action cannot reuse an entry already assigned to a report in this process. Receipts
  are kept for 30 seconds, capped at 4096; a full receipt cache leaves attribution
  unavailable rather than evicting a still-live identity.
  Observations are recorded before handler database work and report admission, so
  skipped or unattributed earlier actions cannot donate delayed entries to a repeat
  action. A later report excludes audit times that could fit an earlier unresolved
  observation of the same guild/user/action; overlapping windows stay unavailable.
  A newer observation also supersedes attribution by an older waiting handler.
  Latest-observation records are process-only, capped at 4096 and kept for 30 seconds.
  If an observation cannot be tracked at capacity, enrichment pauses for that interval;
  report delivery continues with unavailable fields. No past reports are retried.
  On a Gateway ban that is not the bot's own echo, and that does not already
  have an active row, the supplied banned-user name is saved with one
  `member_ban` event before the bounded audit lookup. A matching entry adds
  the moderator ID and actual reason to that same row. Without a reliable
  match, that new row says "Reason unavailable"; older rows keep their recorded
  text. A bot echo that finds no active row stores that one row, marked with
  the operation id of that ban attempt, and does not add a `member_ban` event.
  The command for that same attempt fills the marked row. A later ban with a
  different operation id ends the older active row and inserts a new one.
  A delivery of a ban the bot already recorded does not insert another row
  and does not replace that row's actor or reason. Rows without a known name
  or reliable moderator show the ID or unavailable moderator in the WebUI.
- Role, voice, nickname, username, and thread reports use the same per-event log
  channel and ignore list. They stay off until that event has a channel. They are
  Discord messages only: no SQL event row, no case, and no message text. A member
  role, nickname, or username line is sent only when this process already cached
  the previous member. Voice join, leave, and move need the Guild Voice States
  gateway intent, which is not a privileged intent. Nickname, username, and member
  role logs use the existing Server Members intent. Thread logs use the existing
  Guilds intent. thread_create says created only when the thread snowflake is
  within 60 seconds of the report; otherwise the same line says joined/visible
  because this gateway event also fires when the bot is added to an existing
  private thread. A voice move ignore matches either the channel left or the
  channel joined. These names are not guild-store log-channel types.
- Startup, routine command-plan execution and test/log-only automod reports use Discord
  channels, without a generic SQL event copy. Delete-only automod reports are transient.
  Plans containing timeout/kick/ban steps retain their existing SQL execution summary;
  that summary is not a claim that every step succeeded.

Serenity's general message cache remains disabled. The existing detector's independent
working state implements bounded exact-text and exact-image matching. Snapshot capture
starts automatically only after a captured guild membership, relevant log destination,
observed content access and storage capacity are available. Set
`bot.message_capture_enabled = false` (or `JMODGUARD_MESSAGE_CAPTURE_ENABLED=false`)
to suspend capture. The bot reads this process setting at startup; change it and
restart the bot before capture stops. The bot excludes its own messages, other bots and webhooks;
delivery filters do not narrow capture. Text is capped at 32 KiB UTF-8,
attachment metadata at 16 KiB, and each encoded snapshot at 64 KiB, with truncation
marked. Pending observations are capped at 4096 items and 16 MiB; overflow marks a
coverage gap. Snapshots do not guarantee Discord report replay. Failed delivery is
not reconstructed from Discord or SQL.
The shard collector admits observations in Gateway order before Serenity spawns
ordinary callbacks. It cannot cover events delivered before collector registration;
those reports state that content is unavailable. A verified cleanup or measurement
failure pauses growing capture for that physical store while bounded deletion and
cleanup continue. Capture resumes only after a successful cleanup, fresh measurement
and normal admission. Turning capture off still consumes confirmed deletions.
Known losses are counted for the affected physical store; unattributed Gateway or
catalog gaps advance the coverage epoch without charging an arbitrary store.

Custom templates in a supported database keep their wording on restart; the readable-log
upgrade updates exact previous defaults once. See
[template customization](TEMPLATES.md#existing-saved-templates). Older installations
require the [fresh database baseline](DATABASE-BASELINE.md); their databases are not adopted.

## Delivery limits

The bot admits up to `log.discord_concurrency` reports (default 8), with a total deadline
of `log.discord_timeout_secs` (default 10). Both have environment overrides documented
in `config.example.toml`. Bot Discord report concurrency is a bot process setting. Routine and startup
report admission never waits, so overload drops those reports rather than building
a content backlog. On graceful shutdown, the bot attempts one system notice
for each cached server with a configured system channel. That notice may wait
for a delivery slot within the same per-notice deadline. All
shutdown notices share a three-second process budget; unfinished sends are
cancelled so the bot can close its Discord shards within the updater's drain
window. There is no application retry or replay queue;
Serenity retains its normal request/rate-limit behavior within the deadline.
The current shared limit does not establish per-guild fairness.

A valid warm configuration cache permits delivery during database failure. An expired,
missing or invalidated configuration still needs SQL; it fails within the deadline.
Delivery failures and drops increment a process counter and produce sparse warnings
without raw content. A timeout can mean Discord received the report; no retry follows. The member-sync pacing commit for a global 429 is scheduled outside `log.discord_timeout_secs` and outside activation cutoff.
Mentions and automatic link previews are disabled. Template expansion stops at the
Discord-sized UTF-16 output budget instead of allocating an arbitrary expanded message.
Discord-channel copies have Discord's own lifecycle, separate from jModGuard storage.

## Transitional data model

Moderation/action records include warnings, bans, timeouts, kicks and reversals where
already implemented. They still use the existing moderation tables and outcome
handling; they are not the complete durable case/evidence system. The best-effort
ban lookup above can save moderator and reason on a retained ban row, but it does
not establish a durable Discord audit source for a selected case. Truthful
partial-action outcomes and selected evidence capture remain work.

The retired general message-retention UI/API field remains removed. The bounded
Members view is described in [MEMBERS.md](MEMBERS.md). Guild stats
report identified moderation counts only. Counts include moderation events and live
warn/timeout/kick/ban automod records, excluding dry runs. New moderation-plan summaries
carry a `moderation: true` classification flag, which does not assert step success. Older
plan summaries have only command/step counts and cannot be classified reliably, so
they are excluded from this aggregate. The Audit Log still returns all event rows in a
supported database, without this aggregate filter. Warns and bans retain their separate
table counts. The fresh schema omits `messages`, `members`, `users` and `mutes`; their
archive helpers are removed. Old internal retention fields have no live collection
effect. Databases from before this baseline are unsupported and are not adopted or
erased. Publish matching frontend/API artifacts. Routing guilds across stores remains
subsequent work.

Local tests cover real report mapping, rendered payload flags/limits, guild routing,
unavailable SQL history tables, overload, timeout/capacity recovery and API/UI removal.
They use a fake Discord delivery boundary; no live provider behavior or resource cost
is established.

Snapshot maintenance drains known expiry/pressure backlogs in bounded batches.
After deleting rows or encountering an error it checks again in 30 seconds; an
idle pass waits 120 seconds. Unchanged health uses a two-minute heartbeat and
empty cleanup refreshes its successful-check timestamp at most every five
minutes. No-op checks use primary read snapshots; actual writes recheck state
under the writer lock. Health transitions, observations and loss counters are
still recorded when they change. These intervals stay within the dashboard's
five-minute health and fifteen-minute cleanup freshness windows.
