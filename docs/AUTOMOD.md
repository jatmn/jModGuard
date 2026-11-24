# Automod

jModGuard ships built-in automod rules. Exact-image and repeated-text spam use
dedicated matchers. Their dry-run setting is the log-only test mode; turning it
off selects delete-and-ban mode. The [Onboarding bot trap](ONBOARDING-BOT-TRAP.md)
checks member events and uses a single kick. Message rules with escalation support:

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
- Params: `window_secs` (10), `max_mentions_in_window` (5).
- Triggers when the current message's @everyone/@here and role mentions, plus
  prior recorded mention-spam events inside the window, exceed the maximum.
  The old `max_everyone_mentions` key has no effect and is no longer seeded.
- Default escalation: `1 → ban` (deletes 1 day of messages).

### 2. `attachment_flood` — exact-image spam
- Detection of a common set of identical image bytes from one author across
  distinct channels. Defaults: 3 channels, 2 common images, 60 seconds.
- Counts alone never trigger. Legacy rows are inactive until converted in the
  rule editor. Test mode logs only; enforcement deletes matched messages and bans.
  Creates and attachment-changing edits participate.
- See [exact-image and repeated-text settings](IMAGE-SPAM.md).
- The same author posting matching image attachment metadata in two channels
  within 10 seconds can trigger this rule immediately. The selected rule window
  can shorten that interval. The same action and exceptions apply.

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
  always allowed. Enter codes, not invite URLs or server IDs, in the rule editor.
  Legacy `allowlist_guilds` values were never used by the matcher; saving the
  editor replaces that ignored key with `allowlist_codes` without converting IDs.
- Default escalation: `1 → delete`, `2 → warn`, `4 → timeout 60m`.

### 7. `velocity` — message flood
- Enabled by default. Counts eligible messages from the same author across
  channels in a server, including ordinary messages that match no other rule.
- Set `window_secs` to 1–600 (default 10) and `max_messages` to 0–1,023
  (default 8). An eligible message adds a strike if the count in the window,
  including that message, exceeds the limit. Messages are counted by when the
  bot received them, so a slow configuration read cannot make spaced messages
  look like a flood. Each message is judged on the messages that arrived before
  it. If the bot finishes an author's messages out of arrival order, a strike
  can be missed, but none is charged to a message that was under the limit. Previously saved integer limits
  above 1,023 do not trigger until the limit is lowered to the supported range.
  Saves that keep such a limit unchanged, such as enabling, dry-run, or a rules
  copy, still succeed; a save cannot set a new limit above 1,023.
- Default escalation: `1 → delete`, `2 → timeout 30m`, `4 → kick`.
  Each threshold fires once per strike bucket. After a final delete, timeout,
  kick, or ban fires, further over-limit messages can be deleted at most once per
  second per bucket, across channels. Dry-run only logs the selected action,
  and the usual exemptions apply. A kick and rejoin keeps the bucket until it
  expires after 10 minutes without a velocity strike. Fallback uses the action
  type reserved at the final threshold. A replacement action type at the
  same threshold takes effect on a fresh strike bucket.
- Upgrading: older versions counted only messages that matched another rule.
  On the first bot start after upgrading, the global velocity rule and any
  server override that still match the shipped default switch to dry-run once,
  including a default the servers only inherited, so they log before they act.
  Turn off dry-run to enforce. Edited rules and new installs keep their
  settings. A server whose configuration is frozen by an interrupted guild move
  keeps its rule unchanged. On an upgraded install, a global velocity rule that
  is later removed (for example by copying rules into global from a server that
  only inherits velocity) also falls back to dry-run, not to live enforcement.

### 8. `account_age` — brand-new accounts
- Params: `min_age_days` (3).
- Triggers when the account is younger than the minimum. Report-only by
  default (`1 → log`) — use it to build evidence, then escalate if you want.

## Actions

`log` · `delete` · `warn` · `timeout` (minutes) · `kick` · `ban`. When several generic rules fire on one message, the **most severe** action
wins (ban > kick > timeout > warn > delete > log). Exact-image and repeated-text
matches never use that generic executor; they log or delete-and-ban through their
own path.

Every ban action offers **Ban** (no history deletion) or **Ban and purge
(previous hour)** in the WebUI. Purge asks Discord's Ban Member API to delete
that member's messages across the server from the previous hour, using
`delete_message_seconds=3600` in the JSON body. Ban sends `0`. After a successful
ban with a deletion window, the bot sweeps the author's remaining tracked
messages within that window. Coverage depends on
[the bot's bounded Gateway observations](IMAGE-SPAM.md#enforcement).
This applies to built-in generic rules, custom rules, Exact image spam and
Repeated text. Matched spam-message deletes still run separately.

Actions store `purge_last_hour` (default `false`). Existing JSON without the field
keeps its behavior: `delete_days: 0` is Ban; `delete_days: 1` still sends `86400`
seconds, through `delete_days: 7` for seven days. The shipped Mention spam default
stays one day. The WebUI shows **Keep existing: delete N day(s)** for these legacy
values and preserves them on an unchanged save. Explicitly selecting Ban or
Ban and purge clears the old days value. `purge_last_hour: true` takes precedence
if a stored action includes both fields. For new Exact image spam and Repeated
text rules, the editor preselects the previous hour when enabling enforcement.

Example stored ban actions (inside a rule's `actions` array):

```json
{"at":1,"action":"ban","delete_days":0,"purge_last_hour":false,"reason":"Spam"}
{"at":1,"action":"ban","delete_days":0,"purge_last_hour":true,"reason":"Spam"}
```

Dry-run never bans or purges. Spam test-mode log actions retain the editor's
ban preference so switching back to enforcement preserves the choice; it has no
effect while the rule is in dry-run. Rules live in stored configuration, not
`config.toml`.

## Custom rules (per server)

Operators can add up to **32** guild-scoped rules with ids `custom:` plus 8–32
lowercase hex characters. New rules are stored **dry-run**; a later edit can turn
them live. Copying rules onto another server uses that same create check: each
custom rule arrives in dry-run, the 32-rule cap applies, and custom rules are
refused for the global scope. Saves go through the bot `UpsertRule` control
operation (the API does not write the row directly).

Each custom rule has exactly **one** action (`delete`, `warn`, `timeout`, `kick`,
or `ban` with the Ban / Ban and purge choice above). Types:

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
delete, timeout, or warn. A pending action carries the dry-run setting from
when the rule matched. Known spam requires the current rule and list before an
action can proceed. For other built-in rules, a failed configuration read uses
the rule's settings from the match, including its exemptions. A live kick or ban
then uses the fallback. Other actions keep their normal paths.
Whitelist your announcement channels and your mod/admin roles so @everyone pings
in them never trigger.

## Dry-run mode

With dry-run on, generic rules evaluate normally but instead of acting post a
`[DRY RUN] would <action>` entry to the configured automod log channel.
When a dry-run rule and a live rule match the same message, the live rule's
action runs and the dry-run match is not logged; a more severe dry-run action
never replaces it. A dry-run match also never stops the member's prefix command
in that message. Dry-run and live keep separate strike ladders, so strikes
counted in dry-run never use up live escalation, and live strikes from before a
dry-run trial still count afterwards within the usual 10 minutes. Message
history used for detection, such as velocity's window and recent mentions, is
shared by both modes. The executor checks the rule's mode once before entering
the live path. If the action proceeds, it is log-only when it was selected in
dry-run or the executor sees dry-run at that check. An in-flight live action
can still run after a later switch to dry-run.
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
