# Logging and data collection

Routine message text and attachment metadata are no longer archived to SQL. Joining,
leaving and talking do not maintain a persistent member/user roster or activity counters.
Basic Discord log delivery is independent of those retired history tables.

## Basic reports

- Updates use only supplied content/attachment fields, guild/channel/message references
  and supplied author identity. An absent field is unknown, not empty. Embed/pin-only
  refreshes are skipped. Reports say "updated"; they do not claim a verified before/after
  diff or deduplicate repeated Gateway observations.
- Single deletions report channel/message ID and disclose unavailable author/content.
  Bulk deletions report channel/count without collecting a transcript or per-ID tombstones.
- Join/leave reports use the delivered user and optional cached member join timestamp.
  Missing join context is unavailable; no historical join count is fabricated.
- Startup, routine command-plan execution and test/log-only automod reports use Discord
  channels, without a generic SQL event copy. Delete-only automod reports are transient.
  Plans containing timeout/kick/ban steps retain their existing SQL execution summary;
  that summary is not a claim that every step succeeded.

Serenity's general message cache remains disabled. The existing detector's independent
working state still needs the planned bounded exact-text/image redesign. Optional rich
before/deleted-content logging is not implemented. Routine reports are not retained for
later recovery, and failed delivery is not reconstructed from Discord or SQL.

Existing saved templates keep their wording, including old seeded defaults. Follow
the [template adoption instructions](TEMPLATES.md#existing-saved-templates) to include
message IDs and attachment counts on an existing installation.

## Delivery limits

The bot admits up to `log.discord_concurrency` reports (default 8), with a total deadline
of `log.discord_timeout_secs` (default 10). Both have environment overrides documented
in `config.example.toml`; Administration controls remain pending. Admission never waits,
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

Existing moderation/action records remain, including warnings, bans, timeouts, kicks
and reversals where already implemented. They still use the old tables and outcome
handling; they are not the complete durable case/evidence system. External audit
attribution, truthful partial-action outcomes and selected evidence capture remain work.

The Members view/endpoint and message-retention UI/API field are removed. Guild stats
report identified moderation counts only. Counts include moderation events and live
warn/timeout/kick/ban automod records, excluding dry runs. New moderation-plan summaries
carry a `moderation: true` classification flag, which does not assert step success. Older
plan summaries have only command/step counts and cannot be classified reliably, so
they are excluded from this aggregate. All existing rows, including routine history and
unclassified plans, remain accessible in the Audit Log. Warns and bans retain their
separate table counts. Old `messages`/`members`/`users` schema and data
helpers remain as temporary scaffolding for the fresh tenant model and existing DB
regressions; services are guarded against calling them. Old internal retention fields
have no live collection effect. This wave leaves migrations and bootstrap unchanged and
does not erase existing records. Publish its frontend and API together; older pages that
submit the removed retention setting must reload. Removing tables from the fresh baseline, routing
guilds across stores, guild erasure and backup exclusions remain subsequent work.

Local tests cover real report mapping, rendered payload flags/limits, guild routing,
unavailable SQL history tables, overload, timeout/capacity recovery and API/UI removal.
They use a fake Discord delivery boundary; no live provider behavior or resource cost
is established.
