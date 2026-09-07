# Automod

jModGuard ships 7 detection rules. Every rule has:

- **parameters** (rule-specific, JSON),
- an **action escalation table** — `at` = strike count at which an action
  fires; the highest-severity fired action is executed,
- **whitelists** (roles / channels / users),
- a **dry-run** toggle (log only, no action),
- **scope**: per-server override or global default.

Strikes are counted per (server, user, rule) in memory with 10-minute
buckets; after 10 minutes of silence the counter resets. This is what keeps
escalation accurate without false positives.

## Rules

### 1. `mention_spam` — @everyone/@here spam
- Params: `window_secs` (10), `max_everyone_mentions` (0 = any is a trigger),
  `max_mentions_in_window` (5, cumulative role/channel/everyone pings).
- Default escalation: `1 → ban` (deletes 1 day of messages).
- The classic spam-bot pattern: message containing @everyone/@here.

### 2. `attachment_flood` — crypto/money-scam image floods
- Params: `window_secs` (60), `min_messages` (2), `min_attachments` (2),
  `min_channels` (2).
- Triggers when a user posts ≥2 messages with ≥2 attachments each **across
  ≥2 different channels** within the window — the signature of scam bots that
  spray channels with images.
- Because it requires *multiple channels*, it does **not** false-positive on
  a user posting pictures in one channel.
- Default escalation: `1 → ban`.

### 3. `new_member_url` — link right after joining
- Params: `grace_secs` (120) — the window after joining.
- Triggers when the member posts a URL or Discord invite inside that window.
- Default escalation: `1 → kick`.

### 4. `badword` — admin regex list, bypass-resistant
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

### 5. `invite` — Discord invite links
- Params: `allowlist_codes` — invite codes (`discord.gg/<code>`) that are
  always allowed.
- Default escalation: `1 → delete`, `2 → warn`, `4 → timeout 60m`.

### 6. `velocity` — message flood
- Params: `window_secs` (10), `max_messages` (8).
- Default escalation: `1 → delete`, `2 → timeout 30m`, `4 → kick`.

### 7. `account_age` — brand-new accounts
- Params: `min_age_days` (3).
- Triggers when the account is younger than the minimum. Report-only by
  default (`1 → log`) — use it to build evidence, then escalate if you want.

## Actions

`log` · `delete` · `warn` · `timeout` (minutes) · `kick` · `ban` (delete_days
0–7). When several rules fire on one message, the **most severe** action wins
(ban > kick > timeout > warn > delete > log).

## Whitelists

Per rule: `whitelist_roles` (role IDs), `whitelist_channels` (channel IDs),
`whitelist_users` (user IDs). Matches skip the rule entirely. Whitelist your
announcement channels and your mod/admin roles so @everyone pings in them
never trigger.

## Dry-run mode

With dry-run on, the rule evaluates normally but instead of acting it posts a
`[DRY RUN] would <action>` entry to the configured automod log channel. It does
not create a SQL audit row. **Recommended workflow:** enable the bot, set all rules to dry-run,
watch for a few days, then switch them live one by one.

## Evidence

Live warn/timeout/kick/ban actions retain an audit event (`events` table, visible
in the WebUI Audit Log) containing the rule, action, reason, and selected evidence.
Dry-run, log-only and delete-only observations report to the configured Discord
channel without a SQL copy. Delivery is best effort and is not replayed after failure.
Existing event rows remain visible; these transitional records do not establish
per-action success. See [logging/data behavior](LOGGING-DATA.md).
