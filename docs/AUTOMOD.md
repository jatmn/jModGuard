# Automod

jModGuard ships 8 rule entries. Exact-image and repeated-text spam use dedicated
matchers. Their dry-run setting is the log-only test mode; turning it off selects
delete-and-ban mode. The other rules support:

- **parameters** (rule-specific, JSON),
- an **action escalation table** — `at` = strike count at which an action
  fires; the highest-severity fired action is executed,
- **whitelists** (roles / channels / users),
- a **dry-run** toggle (log only, no action),
- **scope**: per-server override or global default.

For the other rules, strikes are counted per (server, user, rule) in memory with 10-minute
buckets; after 10 minutes of silence the counter resets. This is what keeps
escalation accurate without false positives.

## Rules

### 1. `mention_spam` — @everyone/@here spam
- Params: `window_secs` (10), `max_everyone_mentions` (0 = any is a trigger),
  `max_mentions_in_window` (5, cumulative role/channel/everyone pings).
- Default escalation: `1 → ban` (deletes 1 day of messages).
- The classic spam-bot pattern: message containing @everyone/@here.

### 2. `attachment_flood` — exact-image spam
- Detection of a common set of identical image bytes from one author across
  distinct channels. Defaults: 3 channels, 2 common images, 60 seconds.
- Counts alone never trigger. Legacy rows are inactive until converted in the
  rule editor. Test mode logs only; enforcement deletes matched messages and bans.
  Creates and attachment-changing edits participate.
- See [exact-image and repeated-text settings](IMAGE-SPAM.md).
- A separate attachment burst can time out the same image posted in two or
  more channels within a few seconds, then ask moderators to release or ban.
  It does not use the exact-image thresholds or account age. See
  [IMAGE-SPAM.md](IMAGE-SPAM.md).

### 3. `repeat_text` — exact trimmed text
- The same author posts the same trimmed string across distinct channels.
  Defaults: 3 channels, 60 seconds, 20 Unicode scalars after trim. Empty text
  never matches. Case and internal whitespace are not folded.
- Independent of the image rule. Test mode logs only; enforcement deletes matched
  messages and bans. A dual match in a short window still attempts one ban.

### 4. `new_member_url` — link right after joining
- Params: `grace_secs` (120) — the window after joining.
- Triggers when the member posts a URL or Discord invite inside that window.
- Default escalation: `1 → kick`.

### 5. `badword` — admin regex list, bypass-resistant
- Patterns live in the Badwords tab. Each is tested against three forms:
  1. the **raw** message,
  2. the **normalized** form: lowercase → unicode NFD (accents decomposed) →
     zero-width characters and combining marks stripped → leetspeak mapped
     (`4/@→a`, `3→e`, `0→o`, `1→i`, `5→s`, `7→t`, `$→s`, `!→i`, `2→z`, …),
  3. the **compact** form: normalized with all whitespace removed, so
     `s c a m` and `s5c@m` both match `scam`.
- Note: `1` maps to `i`; if you need both readings use a class, e.g.
  `wor[il]d`.
- Default escalation: `1 → delete`, `2 → warn`, `4 → timeout 60m`.

### 6. `invite` — Discord invite links
- Params: `allowlist_codes` — invite codes (`discord.gg/<code>`) that are
  always allowed.
- Default escalation: `1 → delete`, `2 → warn`, `4 → timeout 60m`.

### 7. `velocity` — message flood
- Params: `window_secs` (10), `max_messages` (8).
- Default escalation: `1 → delete`, `2 → timeout 30m`, `4 → kick`.

### 8. `account_age` — brand-new accounts
- Params: `min_age_days` (3).
- Triggers when the account is younger than the minimum. Report-only by
  default (`1 → log`) — use it to build evidence, then escalate if you want.

## Actions

`log` · `delete` · `warn` · `timeout` (minutes) · `kick` · `ban` (delete_days
0–7). When several generic rules fire on one message, the **most severe** action
wins (ban > kick > timeout > warn > delete > log). Exact-image and repeated-text
matches never use that generic executor; they log or delete-and-ban through their
own path.

## Custom rules (per server)

Operators can add up to **32** guild-scoped rules with ids `custom:` plus 8–32
lowercase hex characters. New rules are stored **dry-run**; a later edit can turn
them live. Copying rules onto another server uses that same create check: each
custom rule arrives in dry-run, the 32-rule cap applies, and custom rules are
refused for the global scope. Saves go through the bot `UpsertRule` control
operation (the API does not write the row directly).

Each custom rule has exactly **one** action (`delete`, `warn`, `timeout`, `kick`,
or `ban` with `delete_days` 0–7). Types:

- **pattern** — `params.pattern` compiled with the same linear-time regex engine
  as badwords (length cap 512; look-around and backreferences rejected).
- **word** — 1–32 whole words matched case-insensitively on literal and
  normalized text. `scam!`, `scam.`, and `(scam)` match `scam`; `scamper` does
  not. A configured word containing punctuation retains its normalized matching.
- **channel_content** — `params.links` and/or `params.attachments` (at least one
  true): URL/invite detection or `attachment_count > 0`.
- **everyone_attempt** — one `@everyone` or `@here` token (or everyone mention
  flag) when the member lacks Mention Everyone in that channel, including role
  and member overwrites and the parent channel of a thread. Active threads are
  read from the guild thread list, then their parent channel. Owning the bot
  application is not a permission. If the channel is not in the guild channel
  list or the active thread list, or a thread's parent channel is not cached,
  the permission is unknown: the message is deleted, a warning is logged, and
  the bot does not kick or ban. A missing guild cache entry or a message with
  no member is unknown in the same way. A cached deny still kicks or bans.
- **honeypot_role** — `params.role_id`; fires when that role is **added** (default
  action kick). Channel include lists do not apply.

Built-in and custom rules share skip lists plus **skip moderators** and optional
**include channels** (empty = all channels except skipped). Custom rules also
expose **post to automod log** and **open a case** switches (built-in rules keep
today’s log/case behavior).

## Whitelists

Per rule: `whitelist_roles` (role IDs), `whitelist_channels` (channel IDs),
`whitelist_users` (user IDs). Matches skip the rule entirely. Optional
`include_channels` limits a rule to listed channels (empty means all non-skipped
channels). `skip_moderators` skips members at mod permission level or higher.
Pending actions recheck moderator and skipped-role exemptions using supplied
member roles or, when those are absent, the author's cached guild roles. A known
exempt author is skipped before deletion, warning records, timeout, kick, or ban.
Exact-image and repeated-text rules honor both settings on admission and again
before enforcement. The message carries its channel, so the channel list cannot
fail open or closed. For a pending kick or ban, unknown moderator status with
`skip_moderators` on replaces the penalty with message deletion and a warning.
Missing membership is distinct from a known empty role list: pending kicks or
bans from rules that skip moderators or roles use that same fallback when member
roles are unavailable. Rules without those role dependencies enforce normally.
An everyone-attempt whose channel permission cannot be resolved uses the same
fallback for kick and ban, including when skip moderators is off. Missing role
definitions also make that permission unknown. Delete, timeout, warn, and log
keep their normal paths without an extra message delete. A delete action runs
once; timeout and warn do not gain a deletion, and log never deletes.
If a custom rule is deleted, or its configuration cannot be read, before a
pending action runs, the bot logs a warning and takes no Discord action, including
delete, timeout, or warn. The pending action does not carry its match-time dry-run
state, so unavailable custom configuration cannot authorize enforcement. A failed
configuration read for a built-in rule uses the fallback only for kick or ban;
its other actions keep their normal paths. Known dry-run rules remain log-only.
Whitelist your announcement channels and your mod/admin roles so @everyone pings
in them never trigger.

## Dry-run mode

With dry-run on, generic rules evaluate normally but instead of acting post a
`[DRY RUN] would <action>` entry to the configured automod log channel.
Exact-image and repeated-text rules report their matched messages there without
deleting or banning. Neither path creates a SQL audit row. **Recommended workflow:**
enable the bot, set all rules to dry-run, watch for a few days, then switch them
live one by one.

## Evidence

Live warn/timeout/kick/ban actions retain an audit event (`events` table, visible
in the WebUI Audit Log) containing the rule, action, reason, and selected evidence.
Dry-run, log-only and delete-only observations report to the configured Discord
channel without a SQL copy. Delivery is best effort and is not replayed after failure.
Existing event rows remain visible; these transitional records do not establish
per-action success. See [logging/data behavior](LOGGING-DATA.md).
