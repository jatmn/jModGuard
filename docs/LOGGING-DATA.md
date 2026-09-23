# Logging and data collection

Routine message text and attachment metadata are no longer archived to SQL. Joining,
leaving and talking do not maintain a persistent member/user roster or activity counters.
Basic Discord log delivery is independent of those retired history tables.

## Basic reports

- Updates use only supplied content/attachment fields and guild/channel/message
  references. A supplied author sets the display name; for ID-only filtering, a
  recent observation of that exact message can supply its author ID when the edit
  omits the author. An unknown author remains unavailable. An absent field is
  unknown, not empty. Discord sets `edited_timestamp` only for an actual edit;
  Gateway refreshes that omit it (embed, GIF/link unfurl, reply preview, pin,
  flags), including full objects that repeat the original content, a
  `message_reference`, or an empty attachment list, are not reports. The bot does
  not report Gateway updates of its own messages, so a log post cannot stack prior
  reports as current text. Each report is one observed edit. Reports say
  "updated"; they do not claim a verified before/after
  diff. A bounded 10-second fingerprint merges partial content/attachment updates
  that share an edit timestamp for the same message in one bot process. A later
  distinct edit timestamp reports again.
- Each guild can save ignored Discord user IDs (including bot IDs) and source channel
  IDs separately for each event in Log Channels. Matching uses the IDs known to
  the report. If no user ID is known, only a source channel exclusion can apply.
  The filter applies before Discord delivery and does not alter moderation
  enforcement or retained moderation records.
- Single deletions report channel/message ID and disclose unavailable author/content.
  Bulk deletions report channel/count without collecting a transcript or per-ID tombstones.
- Join/leave reports use the delivered user and optional cached member join timestamp.
  Leave reports also use join timestamps already supplied with recent messages when
  the member cache is missing that user. This fallback retains only guild/user IDs
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
  On a Gateway ban, the supplied banned-user name is saved before the bounded
  audit lookup. A matching entry adds the moderator ID and actual reason to
  that same row. Without a reliable match, a new row says "Reason unavailable";
  older rows keep their recorded text. Rows without a known name or reliable
  moderator show the ID or unavailable moderator in the WebUI.
- Startup, routine command-plan execution and test/log-only automod reports use Discord
  channels, without a generic SQL event copy. Delete-only automod reports are transient.
  Plans containing timeout/kick/ban steps retain their existing SQL execution summary;
  that summary is not a claim that every step succeeded.

Serenity's general message cache remains disabled. The existing detector's independent
working state implements bounded exact-text and exact-image matching. Optional rich
delete reports may include a snippet this process recently observed; they do not
reconstruct history from Discord or SQL. Routine reports are not retained for later
recovery, and failed delivery is not reconstructed from Discord or SQL.

Custom templates in a supported database keep their wording on restart; the readable-log
upgrade updates exact previous defaults once. See
[template customization](TEMPLATES.md#existing-saved-templates). Older installations
require the [fresh database baseline](DATABASE-BASELINE.md); their databases are not adopted.

## Delivery limits

The bot admits up to `log.discord_concurrency` reports (default 8), with a total deadline
of `log.discord_timeout_secs` (default 10). Both have environment overrides documented
in `config.example.toml`. Conductor Administration can store owner-notification and
monitoring budgets after operator login; bot Discord report concurrency remains a bot
process setting until a later credential-delivery feature. Admission never waits,
so overload drops reports rather than building a content backlog. There is no application
retry or replay queue; Serenity retains its normal request/rate-limit behavior within
the deadline. The current shared limit does not establish per-guild fairness.

A valid warm configuration cache permits delivery during database failure. An expired,
missing or invalidated configuration still needs SQL; it fails within the deadline.
Delivery failures and drops increment a process counter and produce sparse warnings
without raw content. A timeout can mean Discord received the report; no retry follows.
Mentions and automatic link previews are disabled. Template expansion stops at the
Discord-sized UTF-16 output budget instead of allocating an arbitrary expanded message.
Discord-channel copies have Discord's own lifecycle, separate from jModGuard storage.

## Transitional data model

Moderation/action records include warnings, bans, timeouts, kicks and reversals where
already implemented. They still use the existing moderation tables and outcome
handling; they are not the complete durable case/evidence system. External audit
attribution for retained records remains separate from the best-effort ban/unban
display lookup above. Truthful partial-action outcomes and selected evidence capture
remain work.

The Members view/endpoint and message-retention UI/API field are removed. Guild stats
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
subsequent work. Local conductor backup excludes pending Discord actions and can
sanitize controlled copies against surviving cleanup intents.

Local tests cover real report mapping, rendered payload flags/limits, guild routing,
unavailable SQL history tables, overload, timeout/capacity recovery and API/UI removal.
They use a fake Discord delivery boundary; no live provider behavior or resource cost
is established.
