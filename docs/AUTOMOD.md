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
them live. Saves go through the bot `UpsertRule` control operation (the API does
not write the row directly).

Each custom rule has exactly **one** action (`delete`, `warn`, `timeout`, `kick`,
or `ban` with `delete_days` 0–7). Types:

- **pattern** — `params.pattern` compiled with the same linear-time regex engine
  as badwords (length cap 512; look-around and backreferences rejected).
- **word** — 1–32 literal tokens matched case-insensitively on normalized text
  (whole tokens, not substrings).
- **channel_content** — `params.links` and/or `params.attachments` (at least one
  true): URL/invite detection or `attachment_count > 0`.
- **everyone_attempt** — one `@everyone` or `@here` token (or everyone mention
  flag) when the member lacks Mention Everyone.
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
