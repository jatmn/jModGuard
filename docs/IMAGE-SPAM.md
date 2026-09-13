# Exact-image and repeated-text spam

`attachment_flood` and `repeat_text` use dedicated bounded detectors. They never
restore the retired attachment-count trigger and never call the generic automod
punishment executor. Other automod rules keep their own configured behavior.

Each rule can run in **test** (log only) or **delete-and-ban** mode. Test mode uses
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

`exact_image_test` remains log-only. `exact_image` with `dry_run` false requires
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
image hashes across the configured number of distinct channels. Extra images, order
and filenames do not matter. Creates and attachment-changing edits participate.
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
same origin/path, no-redirect, no-credential, no-retry contract as before.

## Enforcement

A verified match in delete-and-ban mode rechecks current configuration, enrollment
and exemptions, then deletes each matched message and bans the author. Deletion and
ban outcomes are recorded separately. A 404 on delete is treated as already gone.
If both rules match the same author in a short window, only one ban is attempted;
remaining matched messages can still be deleted. Cases store proving message IDs,
hashes or trimmed text. Test reports stay Discord logs only.

Private Discord attachment *object* capture into the evidence backend remains later
work; selected case text records the proving posts.

## Resource controls

Image process budgets are unchanged (`[image_spam]` / `JMODGUARD_IMAGE_SPAM_LIMITS`).
Text matching uses independent bounded RAM (512 authors, 64 per guild, 16 posts,
4 MiB metadata). There is still no background task queue of Message payloads.

## Logging

Basic Discord log delivery remains. This process may remember a bounded recent
snippet of observed message text (64 per guild, 10 minutes, 1000 characters) so a
delete report can include supplied content when this process saw the create or
content update. It is not a general message cache, SQL archive or REST history fetch.
Missing snippets report that content was not supplied to this process.
