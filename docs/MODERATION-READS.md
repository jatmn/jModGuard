# Membership-bound moderation reads

Audit events, ban history and moderation statistics now verify the captured bot membership
in the same primary database snapshot as their returned data. A departure or rejoin between
API admission and that read rejects the original request instead of rebinding it.

## HTTP and browser contract

The following GET routes require fresh Discord Manage Guild permissions, active bot
membership and the `x-jmodguard-enrollment` value from the page's loaded configuration:

- `/api/guilds/{id}/events` (also POST for the browser). It retains the `event`
  and `user` filters. Supply any shared list parameter (`sort`, `dir`, `after`,
  `subject`, `actor`, `q`, `from`, `to`) to get `{events, names, next_cursor}`.
  Sort keys are `date`, `event`, `user`, and `actor`.
  The default page order is `date` with `dir=desc` (newest first). Row ID breaks
  ties in the selected direction. Missing user and actor IDs come first in
  ascending order. Pass `next_cursor` as `after` with the same guild, membership,
  filters, sort, and direction. Each request defaults to 50 rows and caps at 250.
  Requests with only the original filters or limit still get an array ordered by
  descending row ID, capped at 200 rows. `subject` (the event user; `user` is
  its older ID-only form, and both together are refused) and `actor` accept a
  positive Discord ID or a name. Events store only IDs, so a name matches the
  member directory's users (nickname, global name, username or mention), at
  most 250 of them; a name filter returns HTTP 400 when the directory is
  unavailable. `q` matches the event name, user and actor IDs, the readable
  `reason`, `previous_reason`, `rule` and `command` payload fields, and up to
  250 directory users whose names match. It does not match other payload
  fields. When the directory is unavailable, `q` still matches the other
  fields without directory names, and the API logs a warning. Letter case is
  ignored for ASCII letters in event text. `from` and `to` are inclusive UTC
  calendar dates. `names` holds directory names for the users and actors on
  the page and is empty when the directory is unavailable.
- `/api/guilds/{id}/bans` (also POST for the browser). It answers
  `{bans, names, total, next_cursor, moderators}`. The default is newest first by ban
  timestamp and row ID. `sort` accepts `subject`, `id` (Discord user ID),
  `reason`, `actor`, `date`, and `status`. Set `dir=asc|desc` and pass `next_cursor`
  as the following request's `after` with the same guild, sort, direction and
  filters. An incompatible cursor returns HTTP 400 with a JSON `error`. Start
  again without `after` when changing filters or using a cursor from before the
  binding was added. The page limit and `skip_metadata` may change between requests.
  Each request caps the response at 250 ban records. Dashboard continuation
  requests can use `skip_metadata=true` to omit the count and moderator lookup.
  `total` counts all matching rows. `moderators` contains actors from the
  guild's full ban history. The filters are `subject` (name or positive
  Discord ID), `actor` (ID), `reason` (literal substring of the current
  reason), `status=active|unbanned`, and inclusive UTC calendar dates `from`
  and `to`. `q` matches stored user and moderator identities and original
  and edited reasons. Names from the member directory are display enrichment.
  Rows keep the original reason and supply the latest edit, editor and edit
  time from the same snapshot. Reason writes keep the indexed row and audit
  event in one transaction.
- `/api/guilds/{id}/stats`.
- `/api/guilds/{id}/analytics?days=7|30|90`, the Analytics summary read: one core
  statement returns the daily series, previous-period totals, rule/kind/moderator
  breakdowns and the weekday/hour heat map; one guild-store statement then returns the
  member-directory size, its import/coverage state and stored names for the listed
  moderators, reported as unavailable when that store cannot answer. Complete answers
  are cached for 30 seconds per membership and range after authorization; other ranges
  are refused.

Guild zero is not supported. Installation ownership does not bypass guild permissions.
Missing or stale page membership returns HTTP409 `guild_enrollment_changed`; an unverifiable
membership returns HTTP503 `guild_access_unverified`. The browser sends the loaded token
without automatically fetching a replacement or retrying the read. Deploy matching API and
frontend builds and reload old pages. String snowflake IDs are unchanged;
event user IDs outside the signed database range are rejected rather than wrapped.

## Database contract and limits

`query_enrolled_events`, `page_enrolled_events`, `page_enrolled_bans` and
`enrolled_guild_stats` take the captured
`GuildEnrollment`. These values are internal scope, not credentials; callers must authorize
the user before using them. The API service guard rejects the old guild-ID moderation reads.
Bot callers retain those helpers. The Bans API page shares the ban row decoder.

Membership verification and data queries share one deferred primary snapshot (`BEGIN
DEFERRED`) and close it with ROLLBACK after the read. That snapshot is still routed to the
primary, and it does not take the writer lock, so a bot write is not queued behind the
read and a held writer does not stall the read. These reads create no recovery
receipts or configuration version changes.
Statistics retain their existing definitions, with all three counts in one
snapshot. Counts use guild-scoped index searches.
Legacy event reads use guild/ID indexes (guild/type/ID for type-only event filters).
Audit Log date pages can seek the guild's UTC millisecond and event ID index
in either direction. Invalid and clock-relative legacy dates use a zero sort key.
An exact event-type filter can seek the guild and type on the existing index
and sort those matching rows. Other sort columns may sort matching guild rows.
Ban pages use guild-prefixed indexes for their selected order. Event queries with
a user filter use the existing user index for both user-only and type-plus-user
lookups, retaining guild and type checks. Counting work still grows with the
guild’s retained records; this change does not establish large-database performance or
distributed read scaling.

Membership fencing does not erase historical guild rows or attach those rows to a particular
installation. After rejoin, a newly authorized page can still read the guild's retained data.
Guild erasure, case/evidence isolation, tenant routing, database credential separation and
missed Discord departure reconciliation remain separate work. Discord permission checks and
SQL are not one transaction. The main configuration bundle already has a captured snapshot;
individual rule, badword, command, template and log-channel GET reads now also have
[captured snapshots](CONFIGURATION-SCOPES.md#individual-configuration-reads).

Local tests cover permission loss, old/missing page tokens, forged/stale scopes, a
membership-changing writer that does not stall the snapshot, empty reads, unavailable
membership storage, guild isolation,
filters, ordering, limits, wire values and receipt/version-free reads. Disposable loopback
libSQL exercises the same captured reads across bot/API connections. These checks do not
claim live Bunny or Discord validation.
