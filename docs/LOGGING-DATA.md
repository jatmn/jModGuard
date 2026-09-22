# Logging and data collection

Routine message text and attachment metadata are no longer archived to SQL. Joining,
leaving and talking do not maintain a persistent member/user roster or activity counters.
Basic Discord log delivery is independent of those retired history tables.

## Basic reports

- Updates use only supplied content/attachment fields and guild/channel/message
  references. A supplied author sets the display name; for ID-only filtering, a
  recent observation of that exact message can supply its author ID when the edit
  omits the author. An unknown author remains unavailable. An absent field is
  unknown, not empty. Embed/pin-only refreshes are skipped. Reports say
  "updated"; they do not claim a verified before/after
  diff. A bounded 10-second fingerprint merges partial content/attachment updates for
  the same message in one bot process. A later distinct edit timestamp reports
  again; a timestamp that arrives after the same untimestamped observation is
  coalesced. Without timestamps, a later change to a field already supplied for
  that edit can trigger another report.
- Each guild can save ignored Discord user IDs (including bot IDs) and source channel
  IDs separately for each event in Log Channels. Matching uses the IDs known to
  the report. If no user ID is known, only a source channel exclusion can apply.
  The filter applies before Discord delivery and does not alter moderation
  enforcement or retained moderation records.
- Single deletions report channel/message ID and disclose unavailable author/content.
  Bulk deletions report channel/count without collecting a transcript or per-ID tombstones.
- Join/leave reports use the delivered user and optional cached member join timestamp.
  Missing join context is unavailable; no historical join count is fabricated.
- Startup, routine command-plan execution and test/log-only automod reports use Discord
  channels, without a generic SQL event copy. Delete-only automod reports are transient.
  Plans containing timeout/kick/ban steps retain their existing SQL execution summary;
  that summary is not a claim that every step succeeded.

Serenity's general message cache remains disabled. The existing detector's independent
working state implements bounded exact-text and exact-image matching. Optional rich
delete reports may include a snippet this process recently observed; they do not
reconstruct history from Discord or SQL. Routine reports are not retained for later
recovery, and failed delivery is not reconstructed from Discord or SQL.

Saved templates in a supported database keep their wording on restart. See
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
attribution, truthful partial-action outcomes and selected evidence capture remain work.

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
