# Membership-bound moderation reads

Audit events, ban history and moderation statistics now verify the captured bot membership
in the same primary database snapshot as their returned data. A departure or rejoin between
API admission and that read rejects the original request instead of rebinding it.

## HTTP and browser contract

The following GET routes require fresh Discord Manage Guild permissions, active bot
membership and the `x-jmodguard-enrollment` value from the page's loaded configuration:

- `/api/guilds/{id}/events`, including event/user/limit filters.
- `/api/guilds/{id}/bans`, including the limit filter.
- `/api/guilds/{id}/stats`.

Guild zero is not supported. Installation ownership does not bypass guild permissions.
Missing or stale page membership returns HTTP409 `guild_enrollment_changed`; an unverifiable
membership returns HTTP503 `guild_access_unverified`. The browser sends the loaded token
without automatically fetching a replacement or retrying the read. Deploy matching API and
frontend builds and reload old pages. Response shapes and string snowflake IDs are unchanged;
event user IDs outside the signed database range are rejected rather than wrapped.

## Database contract and limits

`query_enrolled_events`, `list_enrolled_bans` and `enrolled_guild_stats` take the captured
`GuildEnrollment`. These values are internal scope, not credentials; callers must authorize
the user before using them. The API service guard rejects the old guild-ID moderation reads.
Bot callers retain those helpers, sharing the same SQL and row decoding with scoped reads.

The existing primary-read primitive starts an IMMEDIATE transaction and closes it with
ROLLBACK after the read. Membership verification and data queries therefore retain the
primary writer lock together. No recovery receipt, configuration version change, new table,
dependency, cache, connection pool or polling task is added. Event/ban results retain their
1–200 row clamp, ordering and filters. Statistics retain existing definitions, with all
three counts in one snapshot. Counting scans can take longer as retained records grow;
this change does not establish large-database performance or distributed read scaling.

Membership fencing does not erase historical guild rows or attach those rows to a particular
installation. After rejoin, a newly authorized page can still read the guild's retained data.
Guild erasure, case/evidence isolation, tenant routing, database credential separation and
missed Discord departure reconciliation remain separate work. Discord permission checks and
SQL are not one transaction. The main configuration bundle already has a captured snapshot;
individual rule, badword, command, template and log-channel GET reads now also have
[captured snapshots](CONFIGURATION-SCOPES.md#individual-configuration-reads).

Local tests cover permission loss, old/missing page tokens, forged/stale scopes, waiting for
a membership-changing writer, empty reads, unavailable membership storage, guild isolation,
filters, ordering, limits, wire values and receipt/version-free reads. Disposable loopback
libSQL exercises the same captured reads across bot/API connections. These checks do not
claim live Bunny or Discord validation.
