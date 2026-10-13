# WebUI

The WebUI is a single-page admin app. On a laptop it is served by
**`jmodguard-api`** (feature `embed-ui`). On Bunny it is built files from `dist/frontend/`
on Storage/CDN; the API container stays private and only answers `/api`,
login, and OAuth. The pages never open SQL from the browser. They call the
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
| `dist/frontend/administration/index.html` | owner Administration and Updates page |
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
| daisyUI | Component styles on both pages | Include the 16 used component families, base styles and light/dark themes; omit the broad optional color and radius utility sheets |
| esbuild, es-module-lexer and jsdom | Build, import checks and real React/DOM regression tests | Build/test only; never loaded by the browser |

Exact versions are declared in `tools/webui/package.json` and pinned transitively
by its lockfile. `tools/webui/style.css` lists the used CSS chunks; update it when
adding a component family. These include alert, badge, button, card, checkbox,
fieldset, hero, input, menu, modal, navbar, select, stat, table, textarea and toast.
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
Headless Listbox is another installed option, but its custom popup retains the
Floating UI stack. React Aria offers lower-level selection hooks that need custom
rendering and state integration. Reusing Headless Select avoids both additions.
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
   (Caching → General; `DisableCookies=false`).

For an initial manual install, upload all five files in the Bunny dashboard (**Browse Files**) with Administration HTML at `/administration/index.html`. GitHub Actions publishes an independent WebUI package on trusted publication builds; it does not upload it to Bunny directly. The owner update action uses only a Storage Zone password after rollout. The older manual publisher handles only the three legacy root files and does not publish Administration. To repeat that legacy upload from a checkout, run
[scripts/publish-frontend-bunny.sh](../scripts/publish-frontend-bunny.sh) with
`BUNNY_STORAGE_ZONE`, `BUNNY_STORAGE_PASSWORD`, `BUNNY_STORAGE_ENDPOINT`,
`BUNNY_PULL_ZONE_ID`, and `BUNNY_API_KEY` in the environment (never git); see
[DEPLOY-BUNNY.md](DEPLOY-BUNNY.md) for meanings. Optional `JMODGUARD_FRONTEND_ROOT`
 selects a non-default built frontend directory. A private ops repo is **not required** to install.

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

`http://localhost:8080` → **Login with Discord** → pick a server. Only servers
where your account has **Manage Server** permission are shown, so one WebUI
instance manages all your servers.
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
The rule editor labels each escalation threshold, action and duration. Commands keep
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
settings, rule, badword, command and template saves apply the authoritative
representation returned by that mutation instead of downloading the complete
configuration bundle again. Multi-section copies and responses without a
representation still reload the authoritative snapshot. Other edited fields stay
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
| **Automod Rules** | per-rule editor: params JSON, action escalation table, whitelists, dry-run, scope, reset-to-global |
| **Badwords** | add/remove regex patterns (server; global if bot owner), live tester; invalid legacy patterns show **not enforced** |
| **Commands** | edit every command (name, aliases, description, permission, cooldown, roles/channels, response template); create custom commands with the **step editor**; delete custom commands |
| **Templates** | edit event templates with live preview; server or (owner) global scope |
| **Log Channels** | map each event type to a channel in that server (ID, `<#id>` mention, or Discord channel URL); queued for the bot; save per-event ignored user/bot IDs and source channel IDs |
| **Audit Log** | retained event history (type, user, actor), including older routine rows; new routine events report to Discord channels |
| **Members** | search the saved bounded directory; inspect known join dates, linked cases and import state; request Pause, Resume, Retry or **Reconcile from Discord** when permitted |
| **Bans** | recorded bot and observed Discord bans with known user name, exact ID, and audit-attributed moderator when available; unknown details remain unavailable; unban is queued for the bot |
| **Bot actions** | latest queued unban/log-channel receipts; cancel unstarted work; force-cancel a stuck log-channel request so a new save can run (does not undo Discord) |
| **Settings** | prefix, bot nickname (persisted immediately, then asynchronously applied/cleared on Discord by the bot; transient failures use persisted bounded backoff and permanent failures are shown for operator action; empty / `!setname` with no args / `/setname` without `name` all clear); global prefix (owner only); **copy config from another server** |

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
The same-origin **Administration** link appears whenever `/api/me` identifies the bot owner. It opens `/administration/index.html` and uses the existing session.

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
POST   /api/guilds/{id}/bans           # same query as GET; SPA bans list
POST   /api/guilds/{id}/unban
GET    /api/guilds/{id}/stats          # guild-scoped aggregates only
POST   /api/guilds/{id}/stats          # same body as GET; SPA dashboard counts
GET    /api/guilds/{id}/actions
POST   /api/guilds/{id}/actions        # same body as GET; SPA bot actions
GET    /api/guilds/{id}/cases
POST   /api/guilds/{id}/cases          # same query as GET; SPA case list
GET    /api/guilds/{id}/cases/{case}
POST   /api/guilds/{id}/cases/{case}   # same body as GET
GET    /api/guilds/{id}/cases/{case}/evidence
POST   /api/guilds/{id}/cases/{case}/evidence  # same body as GET
GET    /api/guilds/{id}/cases/{case}/history   # POST on this path adds a note
GET    /api/guilds/{id}/case-delete-roles
POST   /api/guilds/{id}/case-delete-roles  # same body as GET; SPA settings load
PUT    /api/guilds/{id}/case-delete-roles
```

Scopes in request bodies: `"scope": "guild" | "global"` (`global` requires
bot owner).

## Auth & sessions

- Discord OAuth2 (scopes `identify guilds`), sessions last 7 days of
  inactivity. Sessions use an **in-memory store on the single API process**
  (`jmodguard_session` cookie): re-login after an API restart. A shared
  session store is required only if you run multiple API replicas.
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
Saves require Manage Server permission and an online bot writer. See
[exact-image detection](IMAGE-SPAM.md#known-bad-image-management) for first-post
matching, dry-run/enforcement behavior and hash-only retention.

### Appearance

Choose Light, Dark, or System from the Appearance selector on the sign-in screen or sidebar. Light uses a pale blue workspace; Dark pairs it with navy surfaces and soft blue accents. The browser remembers this preference across reloads and sign-ins. System follows your device appearance, including changes while the page is open. If browser storage is unavailable, the choice still applies for the current page. daisyUI widgets reuse those same tokens: cards and stats use `--panel`, hero and tinted surfaces use `--hero`, and primary actions use `--accent`. They do not switch the workspace to daisyUI’s default palette.
