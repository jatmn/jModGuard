# Member directory and import

The Members page is a guild-scoped directory of current members and people with
confirmed ban history. It stores Discord user IDs, current/last-observed names,
known membership stays, and references to selected moderation cases. It does not
archive messages or infer activity from idle members. Current members retain their
known join date indefinitely. An ordinary departure without retained ban history
becomes eligible for cleanup after a five-minute grace period. A confirmed ban
retains known join context and case history through unban, rejoin, and later leave.

## Initial import

Each new bot identity and guild enrollment gets one initial import. The bot
lists existing members itself; administrators do not supply a member list. The
default page is 100 members, and requests from the new sync worker are spaced
at least 15 seconds apart across its guilds. A guild with roughly 11,000 members
needs about 110 populated pages plus a terminal request, so the member phase
takes roughly 28 minutes at the default pace, plus latency, other guilds,
current bans, retries, and any available history recovery. The Members status
card shows committed progress and distinguishes this estimate from known totals.
While the page is visible, the card keeps following an acknowledged sync
control and separate audit recovery even if the initial import is complete.
The member-stage finish time is a preliminary forecast from a recent bot
initialization with a known guild count, then anchored to committed pages and
the shared request pace across active guilds. If progress
becomes too old to support that forecast, the remaining time and finish time
become unknown. An unconfirmed worker is not labeled as crashed. A completed
member stage uses the actual number traversed rather than retaining the
starting guild estimate.

An unfinished import resumes from its durable cursor after restart. A paused
import stays paused. Completion is durable for the bot identity and enrollment:
source commits, image releases, schema upgrades, restarts, and redeployments do
not start another initial import. An empty directory or missing job metadata is
not permission to reseed. Retry on a completed initial import is a no-op. After
completion, a full current-member and current-ban scan runs only when a guild
administrator explicitly selects **Reconcile from Discord**. An outage or
restore can leave coverage uncertain; the card reports that gap until a
requested reconciliation finishes. Live events and bounded checks for affected
users continue while coverage is uncertain. The bot separately catches up
available ban and unban audit actions after a Gateway gap using the same paced
request budget; this audit-only recovery does not list all members or current
bans and does not change the original initial-import completion time. The card
reports its ban/unban stage, checked-entry count, waiting reason, and next
attempt separately from the completed initial import.

The bot uses its existing Discord HTTP client and rate limiter. Current ban-list
entries establish present ban state, identity, and any supplied current reason,
but Discord does not return the original ban or join date in that list. Imported
roster rows carry the actual join timestamp supplied by Discord. Older events
unavailable locally or beyond
Discord's audit retention remain unknown. A snapshot observation never becomes
a falsely dated ban or join action.

## Search and history

The page searches saved user IDs, pasted mentions, display names, and usernames;
no per-search Discord request runs. Name fallback is server nickname, global
display name, username, then ID. Results can be filtered by current membership,
current ban, previous ban, and known join-date range (UTC calendar dates).
Displayed timestamps use the viewer's local time zone. History is paginated and
links selected actions to canonical cases. Distinct repeated bans and unbans
remain distinct actions when their identities are known. Unknown action dates
and incomplete coverage are labeled as such. An unlinked Gateway ban/unban
observation is labeled separately from an audit-ID verified action; a later
audit record may describe the same Discord change. The directory does not
merge them by matching user and nearby time or count them as two verified
actions. Bot-owned actions retain their existing exact operation-to-audit
source links.
An idempotent bot unban response saying the person was already unbanned remains
a successful control receipt but is recorded as an attempted case, not proof
of an earlier ban. Older application unban cases without that response detail
cannot establish retained ban history by themselves; confirmed ban cases and
dated audit actions still do.

When an explicit reconciliation confirms that a previously saved member or
current ban is absent, the history shows a state-reconciliation observation.
The time of the actual departure or unban remains unknown; the separate check
time says only when Discord's current state was verified. An ordinary live
rejoin keeps only its current stay, while a person with confirmed ban history
keeps earlier known stays.
When a tokenless remove arrives for a rejoined member, one paced user check
resolves the membership instance. A confirmed absence closes that saved stay
at the original remove event time and supplies its saved join date to the
departure message; a still-present or newer stay is not reported as departed.
The queued check survives a bot restart. A ban observation alone can establish
that the person is no longer a current member but cannot invent the departure
time; a later matching remove may provide it.

The API requires fresh Manage Guild authorization, the WebUI's captured
`x-jmodguard-enrollment` token, and a live tenant-store scope for reads. Sync
controls are submitted as typed bot actions with stable request IDs; the bot
rechecks actor permission before applying them. A read-only preview cannot
request sync work. Member IDs are JSON strings.

## Retention and deployment

Member profile and stay ownership belongs to the guild store. Case reasons and
evidence stay in canonical case storage; directory history links those cases
without copying private payloads. Authorized case removal deletes its projection
from member history. Guild erasure removes the directory with the existing
enrollment erasure policy. Supported guild moves and backups preserve the
initial completion record and retained history. A deliberate fresh database is
a new installation and cannot restore history that was not backed up.

Independent installations normally use distinct bot identities and databases.
Running the same Discord token against separate databases needs operator-level
Gateway and pacing coordination; one database's worker lease cannot coordinate
another database.

## Storage sizing

The final v9 guild schema was measured in local SQLite with 4 KiB pages and
`VACUUM`. The measurement includes the nullable stay identity on paced
target checks; these bulk fixtures have no pending checks. An empty guild store
measured 258,048 bytes. With one saved stay per
current member, 11,000 ordinary-name members measured 7,872,512 bytes total
(7,614,464 bytes above the empty store). A fixture using long Unicode global
names and nicknames measured 12,173,312 bytes total (11,915,264 bytes above
empty). Adding two extra stays, four case-index identities, and one unknown-time
state observation for each of 110 retained banned people raised the ordinary
fixture by 212,992 bytes to 8,085,504 bytes total.
Canonical case payloads, WAL files, backups, and provider replication are
outside these directory/index measurements.

At the planning input's net growth of 3,285 current members per year, a
43,850-member ordinary-name fixture, representing ten years at that rate,
measured 30,642,176 bytes total (30,384,128 bytes above the empty store).
The difference from the same-name 11,000-member fixture is 22,769,664 bytes
over ten years, or about 2.28 MB (2.17 MiB) per year under the same row mix
and indexing. Churn and retained ban/case frequency can change that rate. The plan's earlier 2.50 MB
ordinary and 3.26 MB Unicode figures excluded final stays and indexes; use
the repaired-schema measured fixtures above for sizing. The earlier candidate
measurements used a different row shape and name generator, so their difference
from these figures is not a pure schema overhead estimate.
