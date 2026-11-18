# WebUI

The WebUI is a single-page admin app. On a laptop it is served by
**`jmodguard-api`** (feature `embed-ui`). On Bunny it is built files from `dist/frontend/`
on Storage/CDN; the API container stays private and the CDN forwards selected
paths to it. The dashboard never opens SQL from the browser. It calls the
API; the API (and the bot) open the database. If the CDN page loads while
the API is still starting, `POST /api/me` (and `GET /api/me`) returns `503` `{error:"not ready"}`
and the UI keeps waiting (up to eight minutes). Consecutive `502`/`504`,
failed fetches, or origin pulls that never complete stop after 90 seconds
of those failures so a bad origin rule is not an infinite spinner. After login,
the SPA loads guild configuration with `POST /api/guilds/{id}/config` (GET remains
for compatibility) so a CDN cannot reuse a cached settings document. `/login` and
`/oauth/callback` return an HTML wait page that reloads until axum can
redirect. OAuth CSRF lives in a cookie, so an API restart between Login
and Discord's return still completes the handshake.

Start `jmodguard-bot` once first so the database is migrated, then start
`jmodguard-api`. Both use the same local SQLite file or remote libSQL database.

## Public policy pages

[Privacy Policy](../crates/api/src/web/legal/privacy.html) is served at
`/privacy` and [Terms of Service](../crates/api/src/web/legal/terms.html) at
`/terms`. The API serves both in either build. They return HTML without
sign-in or JavaScript. Both pages and the dashboard use the same public origin
for navigation and styling. The API fills in the operator details at request
time. It also exposes them in the public `GET /api/legal` response.

When the dashboard is hosted separately, forward `/privacy` and `/terms`
from the public dashboard origin to the API, including when
`JMODGUARD_API_BASE` is set. Keep serving the home page and stylesheet on
the dashboard origin. See the [edge rule example](DEPLOY-BUNNY.md#5-point-api-paths-on-the-pull-zone-at-the-api-container).
Before registering the URLs with Discord, deploy the API and WebUI, then
verify that the public `/privacy` and `/terms` URLs each return their own HTML
with HTTP 200 in a signed-out window. Follow each page’s link to the other.

Fill in the API’s [`[legal]` configuration](CONFIGURATION.md#public-legal-details)
before publishing the policy URLs. Before acting on a deletion request, verify
that the requester controls the Discord account the request is about and use
that account’s Discord user ID. Review its live member-directory and message-snapshot data
across the hosted guild stores.
Coordinate the manual removal with the operator responsible for the bot’s
guild-store writes so queued work and member sync are accounted for. Preserve
moderation cases and their history. Confirm what was removed and what was
retained to the requester privately. The policy gives the initial response
target and explains that later Discord events can recreate entries.

## Getting and hosting the files

Build the five website files from source before hosting or compiling the API's
default `embed-ui` feature (Node.js as supported by `tools/webui/package.json`):

```bash
npm ci --ignore-scripts --prefix tools/webui
npm run build --prefix tools/webui
```

Rebuild after editing `frontend/` or `tools/webui/`. Output is in
`dist/frontend/`, ignored by Git. GitHub Actions runs these steps before checks and
release packaging. The bot and API `--no-default-features` builds need no WebUI build.
Do not upload the unbuilt `frontend/` sources.

The five generated files are:

| File | What it is |
|---|---|
| `dist/frontend/index.html` | the page |
| `dist/frontend/app.js` | the app |
| `dist/frontend/style.css` | the look |
| `dist/frontend/administration/index.html` | owner Administration page: Updates |
| `dist/frontend/administration.js` | update controls and version display |

GitHub Actions also packages these files as an independent `jmodguard-webui` GHCR image. See [independent Bunny updates](BUNNY-UPDATES.md) for owner access, the Storage publisher, and rollout gates.
The public source install does not upload a separate frontend to Bunny; an operator chooses the initial hosting path.

The dependency audit keeps the libraries that supply existing behavior:

| Dependency | Used for | Build decision |
|---|---|---|
| React and ReactDOM | Rendering and component lifecycle on the moderation and Administration pages | Retained; replace manual DOM construction, reconciliation and event wiring |
| Headless UI | Native selects, editor dialogs, focus/dismissal and form label associations | Include Dialog, Field, Label, Input, Select and Textarea plus their internal helpers; exclude Combobox, Menu, Tabs, Listbox and other components |
| Floating UI | Upstream Headless popup dependency | Excluded from browser assets; retained only in the installation lockfile |
| React Aria and its support packages | Headless control focus/hover handling | Retained internal dependencies |
| TanStack Virtual | Upstream Headless Combobox virtualization dependency | Excluded from browser assets; retained only in the installation lockfile |
| daisyUI | Component styles on both pages | Include the 17 used component families, base styles and light/dark themes; omit the broad optional color and radius utility sheets |
| esbuild, es-module-lexer and jsdom | Build, import checks and real React/DOM regression tests | Build/test only; never loaded by the browser |

Exact versions are declared in `tools/webui/package.json` and pinned transitively
by its lockfile. `tools/webui/style.css` lists the used CSS chunks; update it when
adding a component family. These include alert, badge, button, card, checkbox,
fieldset, hero, input, menu, modal, navbar, select, stat, table, textarea, toast and toggle.
The four used theme utilities (`bg-base-100`, `bg-base-200`, `text-warning`,
`rounded-box`) live with the existing token mappings in `frontend/style.css`.
This avoids loading all color/opacity and directional/responsive radius variants
without adding a CSS pruning tool or replacing framework widgets.
`tools/webui/components.js` selects component entries inside the pinned Headless
package because its public barrel retains unused component initializers. A
dependency upgrade must preserve those paths or update the adapter and pass the
component exclusion check. No third-party source is patched or copied into Git.
`dist/webui-metafile.json` records every build input/import and emitted-byte
contribution for auditing; it is not published. Generated JS/CSS and library
licenses share the existing assets. Browsers fetch no external module server
or stylesheet, and no generated bundle is committed.
The old `JMODGUARD_ESM_BASE` and `JMODGUARD_DAISYUI_HREF` overrides are retired.
Both pages use React throughout; there is no parallel vanilla renderer. React's `useSyncExternalStore` subscribes the two roots to immutable UI
status snapshots from the existing API controllers. Each root mounts once;
controllers update state instead of calling `root.render` after each change.
Synchronous controller guards still prevent duplicate submissions between awaits.
Configuration drafts use controlled React inputs keyed by record and field,
with React context identifying each save group. Partial saves retain sibling
edits without scanning DOM inputs, assigning positional IDs or replaying events.
Retry retains the captured guild, tab and membership; navigation discards drafts.
The shared appearance hook retains the session preference even without storage. API permissions, membership fencing, queued receipts and moderation
policy remain application logic. Headless UI owns dialog focus restoration,
Escape/outside dismissal and field label associations. Selects use native browser
menus and keyboard selection, styled by daisyUI; inline text filtering is removed.
The sidebar **Your server** switcher is the one exception: native options cannot
show images, so it is a small first-party listbox in `frontend/app.js`. Its trigger
is a button with the compact daisyUI select size and chevron; the open list is a
daisyUI menu. Each server shows a 20px round Discord CDN icon (the same
`serverIconUrl` source as the owner Bot servers page, using the guild `icon` hash
`/api/me` already returns) or a letter avatar when it has no icon or the image
fails to load; long names end in an ellipsis with the full name as a tooltip.
Arrow keys, Home and End move the active option (`aria-activedescendant` on the
focused `role="listbox"`), Enter or Space selects, Escape closes and returns focus
to the trigger, and Tab or an outside click closes. Opening it makes no API or
Discord request; choosing another server runs the same unsaved-changes check,
draft reset and configuration reload as before.
The rule editor's member search is not a combobox either: matches appear as a
short list of buttons in a daisyUI menu below a plain text input, and role and
channel pickers are native selects. Chosen items are daisyUI badges with a remove
button.
Headless Listbox is another installed option, but its custom popup retains the
Floating UI stack. React Aria offers lower-level selection hooks that need custom
rendering and state integration. Reusing Headless Select for form fields and the
small custom switcher avoids both additions.
TanStack and Floating UI are required, non-optional dependencies of the pinned
Headless UI package. Removing their lock entries or installed directories is not
a supported removal: the next `npm ci` must install that declared graph. Their
absence from browser output is checked by the packaged dependency tests.

Status messages use daisyUI alert variants; toast positioning and field labels
use the toast and fieldset sheets. Setup actions and command-step panels reuse
button/card foundations (including rule escalation panels), with only the existing palette and geometry adjustments.
Obsolete `*-bordered` and unavailable shadow utility classes are omitted.
Pale blue light and navy dark colors stay the existing theme tokens.
Buttons, cards, hero, stats, badges, tables, inputs, and selects
use daisyUI component classes mapped onto
those tokens. First-party CSS is the workspace shell and theme, not a
restyle of daisyUI widgets back into the previous custom controls.

Both themes share one desktop admin-console density, set by tokens in
`frontend/style.css`: 14px body text (`--ui-font-size`), daisyUI
`--size-field` for 30px buttons, inputs and selects (24px `sm`),
`--size-selector` for 24px checkboxes and badges, 16px card padding
(`--card-p`), 12px section gaps (`--ui-gap`) and 20px stat numerals
(`--stat-value-size`). `--target-min` keeps every control, including `xs`
buttons, square icon buttons and info-tooltip triggers, at least 24px
(WCAG 2.2 target size), and all controls share one visible focus ring.
Narrow (720px or less) and touch screens scale fields back up to 40px.
Workspace tabs open with a single header line instead of hero cards; status
panels for loading, server discovery and configuration errors keep a compact
hero. The Dashboard
hides Getting started once the loaded configuration has at least one enabled
automod rule, one enabled command and one routed log channel (one per setup
step); missing collections count as incomplete. Its stat explainer is an info
tooltip: a focusable button described by the tooltip text, shown on hover or
focus, kept open while the pointer moves onto its text, and dismissed
with Escape wherever focus is. `scripts/frontend-contrast-test.mjs` guards
the scale, focus ring, target sizes and colors; the browser check in
`scripts/frontend-state-test.mjs` measures the Dashboard at 1280×800.

Use the **same git tag or commit** as the bot and API when the page needs
new endpoints. CSS-only updates can ship without a new image.

**Self-host (one machine, [SETUP.md](SETUP.md)).** You do not copy these
files anywhere. After the frontend build above, a normal `jmodguard-api` build includes them and
serves them at `http://localhost:8080`.

**Bunny (first-class cloud path, [DEPLOY-BUNNY.md](DEPLOY-BUNNY.md)).**

1. Create a Bunny **Storage Zone**. The create form requires a primary
   region and at least one replication region; do not skip replication.
2. Put the five generated files at their paths in that zone so the root has
   `/index.html` (or `/`), `/app.js`, `/style.css`, and `/administration.js`,
   with `/administration/index.html` in its subdirectory.
3. Create a **Pull Zone** whose origin is that Storage Zone. The public
   admin site is either the Pull Zone `*.b-cdn.net` hostname or an owned domain/subdomain
   you add on that zone. Use that same host for `JMODGUARD_WEB_BASE_URL` and
   the Discord OAuth redirect.
4. Point `/api`, `/login`, `/logout`, `/oauth`, and `/health` on that Pull
   Zone at the API container with **Override Origin URL** (`OriginUrl`) to
   the API endpoint Pull Zone host (you must create a CDN endpoint on the
   API container's port in the app first; sticky sessions are set on that
   endpoint after it exists, not on the add-endpoint screen; see
   DEPLOY-BUNNY.md). Do not
   use Override Origin: Magic Containers on a Storage Pull Zone. Leave
   `window.JMODGUARD_API_BASE` unset. Strip Response Cookies must be off
   (Caching → General; `DisableCookies=false`). Add the separate public policy
   rule for `/privacy` and `/terms` described in [DEPLOY-BUNNY.md](DEPLOY-BUNNY.md#5-point-api-paths-on-the-pull-zone-at-the-api-container).

For the initial bootstrap only, upload all five files in the Bunny dashboard (**Browse Files**) with Administration HTML at `/administration/index.html`. After that, GitHub Actions **Publish** builds an independent WebUI package to GHCR on trusted publication builds; it does not upload it to Bunny directly. The owner installs it from **Administration → Updates**, and the API writes it to Storage with `BUNNY_STORAGE_ZONE`, `BUNNY_STORAGE_PASSWORD`, and `BUNNY_STORAGE_ENDPOINT`, then purges the Pull Zone when both `BUNNY_PULL_ZONE_ID` and a scoped `BUNNY_PURGE_API_KEY` (where the Bunny account offers scoped keys; otherwise purge from the dashboard) are set on the API container (never git); see
[DEPLOY-BUNNY.md](DEPLOY-BUNNY.md) and [BUNNY-UPDATES.md](BUNNY-UPDATES.md#storage-zone-publication). A private ops repo is **not required** to install.

**Another host or CDN.** Copy the five generated files to the site root
(match the API revision when the page needs new endpoints; CSS/layout-only
files can be newer). If the API is on a **different** hostname than the pages,
set `window.JMODGUARD_API_BASE` to the API origin before `app.js` loads
in `index.html`, and set the same value before `administration.js` loads
in `administration/index.html`. Each HTML page needs its own setting;
the value from `index.html` does not carry to the Administration page.
Set `JMODGUARD_CORS_ORIGINS` on the API to the page origin. Discord
OAuth still happens on `web.base_url`: for this separate-host setup, set
`JMODGUARD_WEB_BASE_URL` to the same public API origin as `JMODGUARD_API_BASE`
and register that API origin's `/oauth/callback` with Discord. `/login`
and `/oauth/callback` must share that hostname so the CSRF cookies Discord
cannot set are sent back on the redirect. Bunny’s designed shape does not
need that split: the Pull Zone is one hostname for both the files and `/api`.

Never put a database URL or token in these files.

## Access

`http://localhost:8080` → **Login with Discord** → pick a server. Servers
where your account has **Manage Server** permission (or owns the server) are
shown, so one WebUI instance manages all your servers.

### Access by Discord role

A Manage Server admin can let moderators use the WebUI without Manage Server:
**Settings → Web UI access by role** maps Discord role IDs to a level for that
server only. Every grant is per server; a role in one server never opens another.

| Level | Includes |
|---|---|
| `dashboard` | Read-only Dashboard, Analytics, Audit Log, Members, Bans, Bot actions and Cases (including evidence downloads). Settings tabs are hidden. |
| `settings` | Everything in `dashboard`, plus edits to automod rules, badwords, known spam, known-bad images, templates, log channels and filters, and bot settings for that server. Commands are visible but read-only. |

Manage Server stays required for commands (a command plan can grant roles and run
bans, kicks and timeouts through the bot), copying configuration, member import controls
(Pause, Resume, Retry, **Reconcile from Discord**), case notes and the role list
itself. Case removal, the Removed records view and case-removal role IDs stay with
Administrators and the server owner. Global configuration, Administration (Updates),
the Owner sections (Bot servers, Database Usage, Platform analytics) and **View as…**
stay with the configured bot owner. Ban, unban and ban-reason edits keep
their own Discord **Ban Members** requirement.

The API checks the user's current roles with Discord on every guild request
(`guilds.members.read`, `GET /users/@me/guilds/{id}/member` with the user's own
OAuth token) and reuses an answer for at most 30 seconds, so a removed role stops
working within 30 seconds. If Discord cannot confirm the roles, the request is
refused. Accounts that signed in before this scope was requested must sign out
and sign in again before role access works. The bot rechecks the actor's roles
from its own Discord data before applying a settings edit or a log-channel
change. Hidden controls are presentation only; the API refuses every request the
level does not include (`403` `guild_access_denied`). Role list changes are
written to the API log with the acting user. Server discovery has a five-second
budget for role checks. If a dependency fails or that budget expires, the WebUI
marks the server list incomplete and offers **Retry server discovery**; it does
not grant access to any unverified server. Successful role reads are cached in a
bounded 10,000-entry cache. Unsaved role-mapping edits survive other Settings saves.
The **Members** tab searches saved member IDs and names without a browser-side
Discord roster request. It shows current status, known join dates, linked
moderation cases and the [durable import state](MEMBERS.md). All its reads
require the loaded page's enrollment token. A read-only preview can inspect
the directory but cannot request Pause, Resume, Retry or **Reconcile from Discord**.

## Workspace and saved changes

The sidebar keeps your server and all moderation sections within reach. On
small screens, navigation becomes a horizontally scrollable bar. Tables scroll
within the workspace, and editors support keyboard navigation.

The dashboard separates setup guidance, protection settings and moderation counts.
Rule badges distinguish enforcement state from global defaults and server overrides.
Every automod rule editor uses one layout ([#621](https://github.com/jatmn/jModGuard/issues/621)):
Enabled and Dry-run toggles in the header, a one-line summary that updates as you edit
(for example "When someone sends more than 5 @everyone, @here or role mentions within
10 seconds: Ban (keep messages). Dry-run: only logs."), then **Trigger**, **Actions**,
**Exceptions** and **Scope** sections, with the channel filter (and, for custom rules,
log and case options) under a collapsed **Advanced**. Only Trigger differs between
rules; its inputs read as one short sentence. Escalation steps are compact rows that
show only the chosen action's own setting: minutes for a timeout, the message choice
for a ban. Save, Reset and Cancel stay visible while the editor scrolls.
Exception and channel pickers show names: roles and channels come from the bot's
latest snapshot of the server (`guild_server_names`, refreshed on Gateway connect and
on every role or channel change), users from the Members directory search. Each choice
is a removable chip; the saved rule still stores decimal ID strings. Role and channel
choices use native selects (channels grouped by category; `@everyone` is not offered).
Before the bot's first snapshot, for global rules, or if names fail to load, the pickers
take pasted IDs, mentions or links instead. Commands keep
names separate from their scope badges. Template cards pair a full-width editor with
a preview and put scope and save controls together below. Log-channel cards separate
the destination from per-event ignore lists; each section keeps its own save action.
These layouts adapt to narrow screens and use the selected light or dark theme.

The selected server and section are remembered in this browser for each signed-in
account and API origin. Restored server IDs are checked against your current
manageable-server list. If a server is no longer available, the first accessible
server is selected. Browser storage is optional; if blocked, the app still works,
but selection will not survive a reload. Settings themselves always come from the
API and are never restored from browser storage.

Use **Save** in an editor to apply changes. While an action is running, controls
are disabled to prevent duplicate saves or switching its target server. If a
configuration refresh fails, the workspace shows **Retry** instead of stale
settings. Server loads ignore out-of-order responses. Navigation warns before
discarding unsaved edits, including action/step additions and removals. Ordinary
settings, rule, badword and template saves apply the authoritative
representation returned by that mutation instead of downloading the complete
configuration bundle again. Multi-section copies and responses without a
representation still reload the authoritative snapshot. Command saves always
reload it: built-in override aliases resolve against the other commands, so
one save can change another command's aliases. Other edited fields stay
marked **Unsaved changes**, including when a field-specific settings action runs or
a failed refresh needs Retry. Log-channel saves immediately show the accepted queued
value, clear the submitted row's dirty state, and reconcile the terminal bot-action
receipt automatically; the queued badge does not claim bot validation has completed.
Drafts remain in memory only and are discarded when
you confirm leaving the workspace.

A Global rule save writes the global rule and clears the current server's override
in one transaction. Retrying an unresolved save retrieves its acknowledgement;
the workspace then refreshes current configuration. If that refresh fails, use
**Retry**. Closing the editor does not undo an acknowledged global write.

## Tabs

| Tab | What it does |
|---|---|
| **Dashboard** | server configuration plus identified moderation records, warns and bans; unavailable counts are explicit |
| **Analytics** | command-center dashboard for one range (7, 30 or 90 days, one selector for every panel): a compact KPI strip (member directory size with a partial/unverified badge, net growth, joins, leaves, warnings, bans, spam hits, mod actions, timeouts), each with its change against the previous period of the same length and a daily sparkline; aligned daily enforcement bars; joins/leaves with a running net-change line; top automod rules, mod actions by type and by moderator (names from the stored member directory); and a weekday × UTC hour heat map. Hover or arrow keys show exact per-day values. One captured membership-bound summary read per range, served from stored records only; the API caches complete answers for 30 seconds per membership and range and the browser reuses loaded ranges for 60 seconds |
| **Automod Rules** | per-rule editor: params JSON, action escalation table, whitelists, dry-run, scope, reset-to-global |
| **Badwords** | add/remove regex patterns (server; global if bot owner), live tester; invalid legacy patterns show **not enforced** |
| **Commands** | form editor with a live summary (Basics, What it does, Who can use it and where, Logging): typed behavior settings, role/channel name pickers, a log event dropdown; custom commands use the **step editor** (one compact row per step); built-ins take per-server overrides (name and behavior kind fixed, permission never below the shipped level) with **Reset to default**; delete custom commands; the bot owner sees a collapsed read-only raw behavior view |
| **Templates** | edit event templates with live preview; server or (owner) global scope |
| **Log Channels** | map each event type to a channel in that server (ID, `<#id>` mention, or Discord channel URL); queued for the bot; save per-event ignored user/bot IDs and source channel IDs |
| **Audit Log** | retained event history (type, user, actor), including older routine rows; new routine events report to Discord channels |
| **Members** | search the saved bounded directory; inspect known join dates, linked cases and import state; request Pause, Resume, Retry or **Reconcile from Discord** when permitted |
| **Bans** | recorded bot and observed Discord bans. Each person shows a letter avatar, bold display name, muted `@username` and their ID as a small copyable chip; names the row lacks come from the member directory. An automod ban's moderator reads "jModGuard (automod: rule)"; a ban recorded with both a moderator and an automod rule shows the moderator with that label beneath, and a missing moderator reads "Unknown". The reason is clamped to two lines and expands on click, a missing reason is a muted placeholder, status is a Banned/Unbanned badge and the time is relative with the exact time on hover. **Ban user** opens a dialog (member search or a pasted ID, reason, message-deletion window of 0, 1, 3 or 7 days, optional minutes until an automatic unban) and queues a real Discord ban for the bot; the dialog follows its receipt for up to a minute and shows the result or refusal, which Bot actions keeps. An edit icon replaces the jModGuard reason inline: Discord keeps the reason it was given, the row keeps showing the original, and each edit is an Audit log `ban_reason_edit` event (who, when, previous and new reason). When the ban row is linked to a case (the operation ID saved on the row, as for bot, automod and Bans-page bans, or the Discord audit entry its ban event recorded, as for bans made in Discord), the edit is also added to that case's history as a reason correction with the same facts; the case keeps its original reason. Each committed linked edit retains its exact source, membership and audit-event identity until its history append is acknowledged. The bot retries pending appends in bounded maintenance batches after a transient store failure, even after another edit, without rewriting the current reason; a retry or lost acknowledgment adds no second entry. A ban with no such link, such as a `!ban` command row, or whose case was removed, keeps only the Audit log event. Unban is queued for the bot and its confirmation names the user |
| **Cases** | filter by action, outcome, subject and moderator (names from the stored member directory); select a row to open its case file in a dialog: header badges, who (subject and moderator or automod, with links to their Members page and other cases), why, a readable timeline, evidence as message cards with image previews and downloads, and Add note / Remove case record (removal never undoes Discord) |
| **Bot actions** | latest queued ban/unban/log-channel receipts with readable ban results; cancel unstarted work; force-cancel a stuck log-channel request so a new save can run (does not undo Discord) |
| **Settings** | prefix, bot nickname (persisted immediately, then asynchronously applied/cleared on Discord by the bot; transient failures use persisted bounded backoff and permanent failures are shown for operator action; empty / `!setname` with no args / `/setname` without `name` all clear); global prefix (owner only); **copy config from another server** |

The **Owner** sidebar group appears only for the configured bot owner and needs no
selected server: **Bot servers** (the server directory), **Database Usage** and
**Platform analytics**. Database Usage shows one card per physical database (core and
each guild store): a health badge from its operating state (OK, Pressure, Capture paused,
Growth stopped, Writes paused, Stale or Unknown), allocated size, share of the 1,000 MB
limit and distance to the next threshold, a 0–1,000 MB bar with the 700/800/900/1,000 MB
marks, used against reusable pages, each guild store's snapshot footprint against its
budget, and one freshness chip. A trend slot stays empty because
usage history is not recorded. **Details** lists every diagnostic value, names unknown
values on one muted line, and offers **Show current breakdown**. An unknown allocation is
drawn as an empty dashed bar, never as free capacity; a failed refresh keeps the last
figures marked stale. See [database capacity](DATABASE-CAPACITY.md). Platform analytics
is the fleet view for a 7, 30 or 90-day range: fleet totals with the change from the
previous range and daily sparklines, every enforcement metric and fleet membership per day
at once, the most and least active servers, the biggest movers, each server's share of
bans, spam hits and mod actions, and a sortable, filterable server table with names, icons,
per-metric share bars and member directory status (importing, unverified or unavailable).

The retired general message/member activity archive and chat-retention controls remain
removed. Saved Members selections reopen the bounded directory described above.
Publish matching API/frontend versions; older pages submitting
retention settings must reload. See [logging/data behavior](LOGGING-DATA.md).

## Global vs per-server config

Rules, badwords, commands, and templates are stored either
at **server** scope or **global** scope:

- global values act as defaults for all servers;
- a server value overrides the global one;
- editors show which scope applies ("server override" / "global" badges).

**Log channels are always per-server.** Discord channel IDs belong to one
server, so they cannot be global defaults or copied between servers. The
Log Channels tab accepts a raw snowflake, `<#id>` mention, or channel URL
as a **string** (never a JavaScript `Number`). The same snowflake parsing
applies to role/user/channel ID fields on Rules, Commands, and Settings.

**Global writes are owner-only.** The API requires the signed-in Discord user
to match `bot.owner_id` (non-zero) for:

- path `guild_id=0` (e.g. global prefix via `PUT /api/guilds/0/settings`);
- request body `"scope": "global"`;
- deleting a global badword row.

`/api/me` includes `is_bot_owner` and `preview` when this
tab is a read-only permission preview. The SPA hides Global scope controls and the
global-prefix card when that flag is false. Guild admins still see global
badwords (read-only) because the bot enforces them. Badword list entries include
`valid` / `error` — patterns that fail compile limits are shown as **not enforced**.

The configured bot owner can open **View as…** for a guild they can manage. That new
tab sends `x-jmodguard-preview` and shows **Preview — changes disabled**. The original
owner tab keeps write access. The bearer is handed to the new tab in a URL fragment
(not a query parameter), remains there as a reload-safe tab marker until explicit exit,
and expires after one hour. Administrator, Moderator, and
Member apply distinct synthetic permission masks to permission-dependent responses;
the owner's live Manage Server permission is still rechecked before every guild read.
Preview never makes `is_bot_owner` true.
The same-origin **Administration** link appears whenever `/api/me` identifies the bot owner. It opens `/administration/index.html` (Updates) and uses the existing session.

The **Settings → Copy configuration from another server** action copies any
subset (everything / rules / badwords / commands / templates / settings) from
one of your other servers into the current one — one-click carry-over between
servers. Log channels are intentionally excluded because their Discord channel
IDs are server-specific. You must have Manage Server on the source guild.

Badwords copy is **validate-then-replace**: the destination is replaced with
the enforceable subset of source patterns. Invalid source patterns are counted
as `skipped_badwords`. If the source has patterns but **none** are enforceable,
the copy is **refused** (HTTP error) and the destination is left unchanged —
including when copying “everything” (the whole transaction rolls back).
Empty source clears destination badwords (intentional empty replace).

## API

The UI talks to a JSON API under `/api/...`; every guild endpoint checks the
session can manage that guild (or bot ownership for global scope):

```
GET    /api/me                         # includes is_bot_owner, preview
POST   /api/me                         # same body as GET; SPA login bootstrap (not CDN-cached)
POST   /api/preview                    # owner-only; returns a tab-scoped preview token
DELETE /api/preview                    # exit preview; owner session is unchanged
GET    /api/guilds/{id}/config
POST   /api/guilds/{id}/config         # same body as GET; SPA workspace load (not CDN-cached)
PUT    /api/guilds/{id}/settings
GET    /api/guilds/{id}/rules/{rule}          PUT/DELETE …
GET    /api/guilds/{id}/server-names   # settings level: bot's latest role/channel names
POST   /api/guilds/{id}/server-names   # same body as GET; SPA rule editor pickers
GET    /api/guilds/{id}/badwords              POST (add) / DELETE /{word_id}
GET    /api/guilds/{id}/commands              POST (create) / PUT|DELETE /{name}
GET    /api/guilds/{id}/templates/{event}     PUT/DELETE …
GET    /api/guilds/{id}/logchannels/{event}   PUT/DELETE …
POST   /api/guilds/{id}/logchannels/{event}   # same body as GET; SPA receipt read
POST   /api/guilds/{id}/copy
POST   /api/guilds/{id}/testregex      # pattern/sample length + compile limits
GET    /api/guilds/{id}/events?event=&user=&limit=
POST   /api/guilds/{id}/events         # same query as GET; SPA audit log
GET    /api/guilds/{id}/bans
POST   /api/guilds/{id}/bans           # same query as GET; SPA bans list ({bans, names})
PUT    /api/guilds/{id}/bans/{ban_id}/reason  # Ban Members; jModGuard reason only, never Discord
POST   /api/guilds/{id}/ban            # Ban Members; queued real Discord ban
POST   /api/guilds/{id}/unban
GET    /api/guilds/{id}/stats          # guild-scoped aggregates only
POST   /api/guilds/{id}/stats          # same body as GET; SPA dashboard counts
GET    /api/guilds/{id}/analytics      # ?days=7|30|90 (default 30): every Analytics panel + member directory size
POST   /api/guilds/{id}/analytics      # same body as GET; SPA analytics tab
POST   /api/platform/analytics         # owner-only, ?days=7|30|90 (default 30): fleet trends, previous-range totals, per-server rows, member directory totals
GET    /api/guilds/{id}/actions
POST   /api/guilds/{id}/actions        # same body as GET; SPA bot actions
GET    /api/guilds/{id}/cases          # query options below
POST   /api/guilds/{id}/cases          # same query as GET; SPA case list
GET    /api/guilds/{id}/cases/{case}
POST   /api/guilds/{id}/cases/{case}   # same body as GET
GET    /api/guilds/{id}/cases/{case}/evidence
POST   /api/guilds/{id}/cases/{case}/evidence  # same body as GET
GET    /api/guilds/{id}/cases/{case}/history   # POST on this path adds a note
GET    /api/guilds/{id}/case-delete-roles
POST   /api/guilds/{id}/case-delete-roles  # same body as GET; SPA settings load
PUT    /api/guilds/{id}/case-delete-roles
GET    /api/guilds/{id}/access-roles       # Manage Server only
POST   /api/guilds/{id}/access-roles       # same body as GET; SPA settings load
PUT    /api/guilds/{id}/access-roles       # {roles:[{role_id,level}], revision}
```

Scopes in request bodies: `"scope": "guild" | "global"` (`global` requires
bot owner).

### Cases list

The GET and POST list routes accept the same query parameters.

| Parameter | Behavior |
|---|---|
| `sort` | `date` (by action time), `case` (case ID), `action`, `subject` (numeric ID), `actor` (numeric ID), or `outcome`. Case ID breaks ties. If only `dir` is supplied, the sort is `date`. |
| `dir` | `desc` or `asc`. If only `sort` is supplied, the direction is `desc`. |
| `action`, `outcome` (alias `status`), `subject`, `actor` | Exact filters. Subject and actor take decimal Discord IDs. |
| `q` | Up to 200 characters of literal substring search over reason, case ID, action, outcome, subject ID and actor ID, with ASCII case folding. |
| `from`, `to` | Inclusive UTC calendar dates in `YYYY-MM-DD` format. Either bound may be omitted. |
| `limit` | Page size, default 50 and clamped to 1–100. |
| `after` | Omit for the first page. Pass the previous page's opaque `next_cursor` unchanged. For live lists, keep the same guild, filters, sort and direction. A mismatch returns HTTP 400. |
| `removed` | `true` lists IDs for cleanup in ascending order and requires Administrator permission. Use only `limit` and `after` with this mode. |

The response includes `cases` (ordered IDs), `summaries` (keyed by case ID),
`names`, `can_delete` and `next_cursor`. A null `next_cursor` means there are
no more rows. When both `sort` and `dir` are omitted, the first page uses
ascending case ID order. Legacy live clients can continue in that order by
passing the last visible case ID as `after`. They can also use the returned
opaque cursor with the same parameters. The dashboard requests `sort=date&dir=desc`
by default.

The list reads current case outcomes. An outcome that settles between requests
can move a case across the cursor when sorting by outcome.

## Auth & sessions

- Discord OAuth2 uses the scopes `identify guilds guilds.members.read`. The
  API stores the Discord user ID, OAuth access token, and refresh token
  (when issued) in a server-side session in memory keyed by the
  `jmodguard_session` cookie. The API uses the session to look up servers and check permissions.
  Signing out clears the session. After 7 days of inactivity a session
  expires and can no longer be used. Expired records can remain in the API
  process’s memory until a restart clears all sessions.
- OAuth CSRF is a host-only cookie compared to Discord's echoed `state`
  (empty or mismatched state is rejected). Successful login rotates the
  session id.
- Session cookies use the `Secure` flag when `web.base_url` starts with
  `https://`. API JSON sets `Cache-Control: private, no-store`,
  `CDN-Cache-Control: no-store`, and `Vary: Cookie`. The page loads the
  signed-in workspace with `POST /api/me` so a CDN cannot reuse a cached
  `GET /api/me` identity after the in-memory session is gone. Guild settings
  load with `POST /api/guilds/{id}/config` for the same reason. Log-channel
  receipt checks and the Bot actions table use `POST /api/guilds/{id}/actions`.
  A failed log-channel receipt reads
  `POST /api/guilds/{id}/logchannels/{event}`. Rules, badwords, commands,
  templates, and the Settings tab already render from that POST config
  document. Dashboard counts, the audit log, bans, case lists, a case,
  and its evidence list also use POST. Case history stays GET because
  POST on that path adds a note. Evidence file download stays GET. Expose
  publicly only behind HTTPS (see SETUP.md §8). On Bunny, API-path edge
  rules must Override Cache Time = 0 in addition to Bypass Perma-Cache;
  see [DEPLOY-BUNNY.md](DEPLOY-BUNNY.md).

### Known-bad images

Known-bad images are managed per server under **Known-bad images**. Search by
hash or note; add a confirmed SHA-256 digest and optional safe label, or remove it.
Saves require Manage Server permission (or a `settings` access role) and an
online bot writer. See
[exact-image detection](IMAGE-SPAM.md#known-bad-image-management) for first-post
matching, dry-run/enforcement behavior and hash-only retention.

### Appearance

Choose Light, Dark, or System from the Appearance selector on the sign-in screen or sidebar. Light uses a pale blue workspace; Dark pairs it with navy surfaces and soft blue accents. The browser remembers this preference across reloads and sign-ins. System follows your device appearance, including changes while the page is open. If browser storage is unavailable, the choice still applies for the current page. daisyUI widgets reuse those same tokens: cards and stats use `--panel`, hero and tinted surfaces use `--hero`, and primary actions use `--accent`. They do not switch the workspace to daisyUI’s default palette.

### Icons

The Web UI draws every icon as an SVG from one shared set in `frontend/icons.js`; it never
uses emoji or text glyphs as icons. The paths are hand-authored for this project, so the set adds
no icon-library dependency or third-party source. Discord template previews still show what the
bot will post, including any emoji in the template.

Each meaning has one icon and one fixed color, the same on every page and in both themes:

| Action | Icon | Color |
|---|---|---|
| Ban, Bans menu | hammer | red |
| Remove or delete | trash | red |
| Remove the bot from a server | leave (door with arrow) | red |
| Unban | person with a check mark (never a padlock) | green |
| Edit | pencil | blue |
| View, open a case or history | eye | blue |
| Download evidence, retry cleanup | download, circular arrow | blue |
| Copy ID | copy | gray |
| Cancel a queued bot action | circled x | gray |

Row and record actions on report pages are compact 24px icon buttons. Each keeps its name as
its accessible label and hover tooltip, its confirmation dialog, and the shared busy/disabled
state; form-footer buttons such as **Save** keep their text. Every sidebar item and Administration
page control shows its icon beside its label (for example a bar chart in purple for Analytics and
teal people for Members); in the sidebar the icon sits on a small tile so it stays readable on
the highlighted current item. The sidebar's own buttons use the same set: **Administration**
(crown, blue), **View as…** (eye, blue), **Exit preview** (back arrow, gray) and **Sign out**
(door with arrow, gray).

Each Dashboard Overview tile shows a small icon before its label, in the icon and color of the
page or action it counts, so a tile matches its sidebar item:

| Dashboard tile | Icon | Color |
|---|---|---|
| Rules enabled | shield (as Automod Rules) | green |
| Rules in dry-run | eye (watches without acting) | blue |
| Commands enabled | terminal prompt (as Commands) | blue |
| Log channels | # (as Log Channels) | gray |
| Moderation records | list (as Audit Log) | gray |
| Warns | triangle with an exclamation mark | amber |
| Bans | hammer (as Bans) | red |
| Cases | briefcase (as Cases) | amber |
| Prefix | sliders (as Settings) | gray |
| Bot name | robot | blue |
| Badword patterns | speech bubble with x (as Badwords) | amber |
| Routine chat collection | database | gray |

The **Getting started** links show the sidebar icon of the page they open, where a text arrow
used to be: **Tune automod** a green shield, **Configure commands** a blue terminal prompt and
**Route your logs** a gray #. Tile and link labels are unchanged, the icons are hidden from
screen readers, and they use the same 16px size as other inline icons. The header actions
**Review protection** and **View audit log** stay text, like other page toolbars.

Icon colors are their own `--icon-*` palette in `frontend/style.css`, separate from the theme's
primary and accent colors. Light and Dark use a lighter or darker shade of the same color, and
every shade keeps at least 3:1 contrast against the surfaces icons appear on.
