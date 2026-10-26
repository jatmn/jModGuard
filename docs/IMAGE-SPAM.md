# Exact-image and repeated-text spam

`attachment_flood` and `repeat_text` use dedicated bounded detectors. They never
restore the retired attachment-count trigger and never call the generic automod
punishment executor. Other automod rules keep their own configured behavior.

Each rule can run in **dry-run** (log-only test) or **normal** (delete-and-ban)
mode. Dry-run uses
the same matcher and never deletes, bans, queues later punishment or creates cases.
Switching to enforcement does not replay old test results. Defaults remain three
distinct channels and a 60-second window. Images also default to two common distinct
files. Text defaults to a 20 Unicode-scalar minimum after trim; that unit is the
scalar count of the trimmed string (`chars().count()`), chosen so short replies such
as `ok`/`thanks` do not match while a typical paste does.

```json
{"mode":"exact_image_test","min_channels":3,"min_attachments":2,"window_secs":60}
{"mode":"exact_image","min_channels":3,"min_attachments":2,"window_secs":60}
{"mode":"exact_text","min_channels":3,"window_secs":60,"min_length":20}
```

The defaults and WebUI store `exact_image_test` for image dry-run. An
`exact_image` rule with `dry_run` true and only `log` actions is also valid and
remains log-only; the mode string alone does not enable enforcement.
`exact_image` with `dry_run` false requires
delete and ban actions (log is optional). The runtime still executes those through
the dedicated spam path. Legacy count-rule rows without a mode stay **inactive**.

Role, channel and user whitelists are per-rule. Own-bot messages are ignored. Other
bots and privileged roles are not implicitly exempt. Exemptions apply at admission
and are rechecked with a fresh member read before enforcement. User and channel
exemptions still apply when an update omits member roles. A configured role
whitelist cannot be evaluated without roles, so that update is not admitted.
Delete and ban claims are reserved while Discord work is in flight and committed
only after success, so a failed or timed-out attempt can be retried in the same
window. If membership cannot be confirmed at enforcement time, the bot logs the
skip and does not punish.

## Matching

**Images:** one author in one guild posts the same common set of distinct SHA-256
image hashes across the configured number of distinct channels. Once attachments
are admitted as images, extra images, order and filenames do not affect byte
matching. Creates and attachment-changing edits participate.
An explicit image media type is enough even when Discord omits dimensions. When
the optional media type is absent, dimensions and a recognized image filename
are required; an explicit non-image media type is not admitted.
Unchanged or attachment-omitted updates do not refresh an image window. A text-only
edit does not reset an image candidate.

**Text:** the same author posts the same trimmed string across the configured distinct
channels. Only leading/trailing whitespace is removed. Case, internal whitespace and
URLs are not folded. Empty text never matches. Image attachments are irrelevant. A
content-omitted update does not prove a text change; an unchanged trimmed string does
not start a new window.

Original Discord create or content-change time maps to monotonic observation time.
Edits replace the physical message's candidate. Deletes invalidate it. Duplicate
creates, expired IDs and late hash results cannot refresh the window.

Hashing still streams original `cdn.discordapp.com/attachments/...` URLs with the
same origin/path, no-redirect, no-credential contract. A completed body within the
file cap is hashed even when it is longer than the size Discord declared, and only
when the process byte budget covers that received length. Each chunk's additional
bytes are charged atomically before hashing, including on retries, so concurrent
streams cannot reuse spare capacity. Charges survive body errors, timeout and
cancellation; the initial declared-size reservation is not refunded. A body that
ends short of that declared size is fetched once more only when the process, guild, and author
request budgets and the process byte budget admit the extra request. A body over
the cap, a body the byte budget cannot cover, and every other download failure, is
not retried.

## Enforcement

A verified match in delete-and-ban mode rechecks current configuration, enrollment
and exemptions, then deletes each matched message and bans the author. Deletion and
ban outcomes are recorded separately. A 404 on delete is treated as already gone.
If both rules match the same author in a short window, only one ban is attempted;
remaining matched messages can still be deleted. Cases store proving message IDs,
hashes or trimmed text. Test reports stay Discord logs only, with no SQL cases and no
object capture.

A verified image match, in either mode, uses a reason that describes the
identical-image match. The automod Evidence field lists the matched channel and
message ids and the proving SHA-256 hashes. It does not include image bytes or
the attachment address. If a download, request budget, or comparison work budget stops analysis before a
match is proven, the automod title is **Exact image check incomplete**, the action
is **no action; check did not finish**, and the Evidence field names the stopping
gate. A size mismatch also names the declared size and the received length. The
field never includes a URL, a hash, or image bytes. A later check in the same incomplete
window can still become a verified hit. An incomplete notice that has not been
sent is not sent once that verified match owns the window. That check runs again
after a Discord rate-limit wait and before the log request is written. A request
already sent is not recalled. A global rate-limit wait still records the member-sync deadline when that notice is later dropped or the delivery is cancelled, and that database write does not use the log-delivery deadline. Replacing a post's attachments clears an incomplete
reservation and leaves a verified window in place. A parked author keeps the
reservation, so the notice is still sent. A new configuration
token clears the incomplete-notice reservation; park and restore on the same token
still keeps it; a verified window is not cleared by the token change. A repeat
incomplete notice is not sent
after one was delivered. If delivery does not succeed, a later incomplete check
in that window can try the notice again. A permanent log-channel rejection
(Discord 50001, 50013, or 10003) still uses the existing 30-minute suppression,
so that later try does not call Discord again; a transient failure, including 429
and 5xx, does. A failed job does not clear hashes
already stored for those posts. Hashing uses one CDN request per attachment
that does not already have a stored hash, plus one extra request only when that
body ends short of the declared size and the process, guild, and author request
budgets and the process byte budget admit it. Redirects are not followed. Image
bytes are not stored.
An overlapping job copies hashes stored since it was admitted before it charges
its download budget. When every live post already has a hash and comparison finds
no match, a budget miss does not send an incomplete notice. Hashes already stored
still count toward a match, and an in-flight result that proves the match is
reported as that match. There is still no background message queue; verified
evidence remains hash-only.

Exact-image matches retain only hashes and case text metadata. They never capture,
cache, or re-share attachment bytes through the evidence backend. Attachment hashing
streams bytes through SHA-256 in memory and discards each chunk. Other evidence
backend functions remain available for their separately authorized uses.

## Known-bad image management

Select a server, then **Known-bad images** in the WebUI. Operators with Manage
Server permission can list/search digests and safe notes, add a confirmed 64-character
SHA-256 from a notice, and remove an entry. A duplicate add preserves the original
metadata and performs no configuration write. Removal of an absent digest is also
a no-op. Changes use the authenticated bot writer link and the page's loaded bot
membership; the API stays read-only. The bot must be online to save.

The list is guild-local, capped at 1024 digests, and independent of configuration
copy/global defaults. It survives restarts. Metadata holds the added time, WebUI
source, an explicitly unknown actor (no retained operator identifier), and an
optional short label without links, mentions, or identifiers. No images or
attachment addresses are accepted by this management API. Invalid/missing hashes
are rejected; restored invalid rows cannot establish a match.

A listed digest matches a single admitted incoming image without waiting for
repetition across channels. The exact-image rule must be enabled; dry-run logs
only and normal mode retains its delete-and-ban behavior. Existing exclusions,
member verification, replay/invalidation and process budgets still apply.
An empty list retains ordinary common-image thresholds. Adds/removals advance the
existing configuration version so in-flight results from older configuration
cannot punish. Normal cross-process configuration polling delay still applies.

Learning is explicit: confirm a digest, then add it in the WebUI. A dry-run match
never adds a digest automatically.

## Cross-channel attachment burst

Process settings `[attachment_burst]` and `JMODGUARD_ATTACHMENT_BURST` watch
one member posting the same attachment in several channels a few seconds
apart. The default is 2 channels inside 10 seconds, then a 10 minute timeout
and a mod-log post with Release and Ban buttons. A finished check matches the
exact content hash. An unfinished check matches size, filename, and content
type, so a body-length mismatch still counts. This does not change the
exact-image size check, does not use that rule's 3-channel exact-hash
threshold, and does not look at account age. Repeats in one channel, posts
outside the window, and other members do not count. A repeated delivery of
the same message does not time the member out again. The timeout is the only
automatic action; the buttons reuse the existing timeout removal, ban, and
case paths. A button posted before a restart does not apply to a later review.

## Resource controls

Image process budgets are unchanged (`[image_spam]` / `JMODGUARD_IMAGE_SPAM_LIMITS`).
Text matching uses independent bounded RAM (512 authors, 64 per guild, 16 posts,
4 MiB metadata). There is still no background task queue of Message payloads.

## Logging

Basic Discord log delivery remains. This process may remember a bounded recent
snippet of observed message text (64 per guild, 10 minutes, 1000 characters)
and up to 1200 bytes of attachment URLs so a delete report can show what this
process saw on create or update. It is not a general message cache, SQL archive
or REST history fetch.
Missing snippets report that content was not supplied to this process.
