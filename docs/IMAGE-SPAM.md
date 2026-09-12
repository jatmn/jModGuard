# Exact-image spam test

`attachment_flood` now has a separate, bounded **log-only** detector connected to
Gateway message creates. It no longer uses attachment counts or other rules' strike
buckets to trigger a ban. It cannot delete, ban, write cases, capture evidence objects
or store message history. Other automod rules keep their own configured behavior.

In the WebUI rule editor, save to select exact-image test mode and configure the
server's `automod` log channel. New installations default to three distinct channels,
two common distinct images and a 60-second window. Server overrides and explicit
user/channel/role whitelists still apply. Existing count-rule rows without the new
mode are **inactive**, as the rule card indicates, until explicitly converted.

The supported parameters are:

```json
{"mode":"exact_image_test","min_channels":3,"min_attachments":2,"window_secs":60}
```

Channels accept 2–10, images 1–10 and the window 1–600 seconds. `dry_run` must be true;
actions must be empty or log-only. The editor saves a single log action. Shared DB
writers and rule copies validate explicit test configurations; the runtime also
refuses unsupported/directly edited rows. `min_messages` is retired for this rule.
The existing `automod` template renders the report; escalation and custom action
reasons are not used by the image test.

## What constitutes a match

One author in one guild must post the **same common set** of distinct SHA-256 image
hashes across the configured number of distinct channels. A+B, B+A+C and A+B match
with the defaults. A+B, B+C and A+C do not. Two copies of A in one message count as
one distinct image. Filenames, attachment order and surrounding text do not affect
byte equality. Resizing, re-encoding or changing the actual file bytes does.

The create must include guild-member roles and image attachment metadata (image MIME
type, width and height). Unknown members, webhooks and incomplete image metadata are
skipped. Own-bot messages are ignored; other bot members and privileged roles do not
receive an implicit exemption. Role/member changes invalidate that author's existing
observations. Message Content intent and the normal bot Gateway intents are required;
this feature does not reconstruct unavailable content through REST/history requests.
See the [Discord message/attachment reference](https://docs.discord.com/developers/resources/message#attachment-object)
and [signed attachment URL reference](https://docs.discord.com/developers/reference#signed-attachment-cdn-urls).

Metadata first gates possible candidates across enough channels. Only then does a
bounded background task fetch original `cdn.discordapp.com/attachments/...` URLs with
matching channel/attachment IDs. It streams SHA-256, checking exact declared length;
there is no image decoder, full-file buffer, alternate proxy URL or new HTTP stack.
The existing Hyper/rustls/ring dependencies are reused. URLs cannot choose other
origins, and requests carry no bot credential, follow no redirects, retry no cancelled
requests and perform no
response decompression. Download errors do not include signed URLs or response bodies.

## Resource controls

Bot-only `[image_spam]` TOML controls (see `config.example.toml`) or the
`JMODGUARD_IMAGE_SPAM_LIMITS` JSON object configure startup limits. JSON replaces the
TOML section and fills omitted keys from defaults. The API ignores these controls.
Invalid bot limits fail startup. These process controls need a bot restart today;
a future owner operations UI is not implemented by this change.

| Control | Default |
| --- | ---: |
| Metadata accounting budget, total / per guild | 4 MiB / 1 MiB |
| Tracked authors, total / per guild | 512 / 64 |
| Posts per author / image files per post | 16 / 10 |
| Maximum file size | 8 MiB |
| Concurrent background jobs | 2 |
| Whole download-job deadline | 10 seconds |
| Reserved requests per minute, total / guild / author | 120 / 40 / 20 |
| Reserved declared download bytes per minute, total | 64 MiB |
| Hash comparison work steps per evaluation | 100,000 |

Budgets bound application work; they are not measured RSS or exact network billing.
Metadata accounting includes URLs and conservative per-record overhead. In-flight
bounded snapshots, HTTP/TLS buffers, hash state and general bot/runtime overhead are
additional. Request/declared-byte budgets are charged before downloads, including
failed attempts. There is no waiting task queue. Jobs retain only bounded attachment
references/hashes/IDs, not the original Message or chat text, and do not delay prefix
commands or ordinary automod. The full job, including config reconciliation/logging,
has the download deadline plus 15 seconds. Log delivery also uses existing limits.

Active references/hashes expire with the rule window (idle cleanup can add up to 60
seconds). Invalidation and configuration-token changes retire those observations
immediately: live author slots, post slots and metadata accounting are reclaimed so
later creates are not blocked by dead entries. Replay watermarks stay in a separate
bounded table (same per-guild author cap) with rate budgets until idle cleanup.
Small author replay watermarks last at most 660 seconds after the last
accepted create, plus the cleanup tick. These are transient RAM state, not SQL rows.
All state is lost on restart. Event work touches one bounded author bucket; the global
expiry scan occurs only on the existing cleanup timer.

## Current coverage and remaining work

- This phase handles **creates only**. Any delivered update, including a partial or
  text-only update, invalidates that message. Delete/bulk-delete invalidates it too.
  Edited messages do not re-enter the detector yet. A callback revision also prevents
  admission when an invalidation arrives while its create is loading configuration.
- Original Discord create time is mapped to monotonic time; duplicate creates, expired
  IDs and late hash results cannot refresh the detection window. Future/old timestamps
  and creates already marked edited are skipped. Clock skew can reduce coverage.
- Changed configuration invalidates old observations at the next admission; queued
  reports recheck the normal bot configuration observation. This is best-effort test
  reporting, **not** the strict fresh-permission/enrollment fence needed for punishment.
- A qualified candidate with failed hashing, exhausted download budget or exhausted
  comparison work produces an **unverified** report, never a claimed match. Reports
  are limited to one per author/guild per window. Admission/metadata/concurrency drops
  instead increment a sparse process diagnostic, without allocating a report backlog.
  New traffic is needed to retry a candidate skipped because all jobs were occupied.
- Explicit capacity limits and conservative invalidation deliberately reduce coverage
  during overload. Local fixtures validate behavior; no live Discord load, CDN behavior,
  production memory usage or provider cost has been measured here.

The next slice must implement changed-attachment edit handling and strict enforcement
revalidation, delete the actual matched messages, ban with truthful separate outcomes,
and retain selected case/evidence records through the tenant lifecycle. It must not
route image matches into the current generic punishment executor. Repeated-text spam
is also separate work. This test-only phase is not production delete-and-ban protection.
