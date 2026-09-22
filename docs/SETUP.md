# jModGuard Setup Guide

**Database compatibility:** this release requires a fresh database or the recognized
`jmodguard-core-1` family. Earlier databases/backups require their matching old release;
they cannot be adopted by this baseline. See [database setup and recovery](DATABASE-BASELINE.md).

A data-driven Discord moderation bot: automod against spam bots, per-server
config with global fallback, editable commands/events/templates, and a WebUI.

This guide covers everything from zero to a running bot on **Linux**
(self-host / VPS). Canonical public-source installation, Bunny, AWS, and
deferred live checks: [INSTALL.md](INSTALL.md). For Bunny.net Magic Containers,
use [DEPLOY-BUNNY.md](DEPLOY-BUNNY.md). For EC2, [DEPLOY-AWS.md](DEPLOY-AWS.md)
(SQLite or libSQL; not RDS).

---

## 1. Prerequisites

- Rust toolchain (rustup): <https://rustup.rs> — the repo pins **Rust 1.96.0**
  via `rust-toolchain.toml` (same as `Cargo.toml` `rust-version` and the
  Docker builder images). `rustup` will select that pin automatically in this
  directory.
- A Discord account (the bot owner).
- Somewhere to run it: a Linux machine that stays online (VPS, home
  server, Raspberry Pi class box is fine).
- While this repository is private: GitHub HTTPS credentials that can read it
  (`gh auth login` + `gh auth setup-git`, or a PAT). An SSH key is not
  enough for the Cargo git patch (see [Build](#5-build)).

## 2. Create the Discord application

1. Go to <https://discord.com/developers/applications> → **New Application** →
   give it a name (this becomes the bot's name; you can also set or clear a
   per-server nickname later via the WebUI Settings tab or `!setname` /
   `/setname` — empty / omit / `clear` / `-` resets).
2. Left sidebar → **Bot**:
   - Click **Reset Token** and copy the token. Treat it like a password.
   - Enable these **Privileged Gateway Intents**:
     - **Server Members Intent** — **required** (member events and moderation)
     - **Message Content Intent** — **required** (message filtering)
     - **Presence Intent** — not used by jModGuard (safe to leave off)
3. Left sidebar → **OAuth2 → General**:
   - Copy the **Client ID** and **Client Secret** (needed for the WebUI login).
4. Left sidebar → **OAuth2 → URL Generator** (or use the invite URL below).

## 3. Invite the bot to your server

Use the **URL Generator** (OAuth2 → URL Generator):

- Scopes: `bot`, `applications.commands`
- Bot permissions (minimum set; add more if you want `!lockdown`-style power later).
  Tick them in this Discord URL Generator order:
  - **General:**
    - `View Audit Log` (moderator/reason details in ban and unban logs)
    - `Kick Members`
    - `Ban Members`
    - `Manage Nicknames` (WebUI nickname apply/clear and `!setname` / `/setname`)
    - `Manage Roles` (only if you use the `add_role`/`remove_role` plan steps)
    - `Moderate Members` (timeouts)
  - **Text:**
    - `View Channels`
    - `Send Messages` (log events, command responses)
    - `Manage Messages` (delete/purge)
    - `Read Message History`

Open the generated URL in a browser, pick your server, authorize.

> **Note:** Once the bot is in **100+ servers** Discord requires
> [verification](https://support.discord.com/hc/en-us/articles/360040720412)
> to keep using the privileged intents. For a handful of your own servers you
> are fine unverified.

## 4. Add the OAuth redirect URI (for the WebUI login)

Discord OAuth2 → **General** → scroll to **Redirects**:

- Add `http://localhost:8080/oauth/callback` if you run it locally, or
  `http://YOUR_SERVER_IP_OR_DOMAIN:8080/oauth/callback` for a remote host.
- It must match `web.base_url` in `config.toml` exactly.

## 5. Build

`Cargo.toml` patches `libsql-sqlite3-parser` by cloning a revision of this
same GitHub repository over **HTTPS**
(`https://github.com/jatmn/jModGuard`). `.cargo/config.toml` only tells Cargo
to use `git(1)` for that fetch; it does **not** rewrite HTTPS to SSH. While
the repository is private, authenticate HTTPS **before** `git clone` and
`cargo build`. An SSH key or `git clone git@github.com:...` does not
authenticate the HTTPS patch fetch unless you also rewrite GitHub HTTPS URLs
to SSH.

```bash
# HTTPS credentials (covers clone and cargo fetch):
gh auth login
gh auth setup-git
# SSH-only alternative (rewrite HTTPS GitHub URLs to SSH):
# git config --global url."git@github.com:".insteadOf "https://github.com/"

git clone https://github.com/jatmn/jModGuard.git
cd jModGuard
# rust-toolchain.toml selects Rust 1.96.0 via rustup.
# Independent bot package (no dashboard stack).
cargo build --locked --release -p jmodguard-bot
# API with the default embedded frontend.
cargo build --locked --release -p jmodguard-api
```

Once the repository is public, skip the authentication commands; a normal
HTTPS clone is enough.

Binaries:
- `target/release/jmodguard-bot` — Discord gateway (applies migrations)
- `target/release/jmodguard-api` — WebUI/API

There are **no system dependencies** — SQLite is compiled in, TLS is rustls.
Start the bot once before the API so the schema is migrated. Select each production
service with `-p` as above. Bare `cargo build --bins` builds all workspace binaries,
including the conductor. For a Bunny API without embedded pages, use
`cargo build --locked --release -p jmodguard-api --no-default-features`.

The optional conductor does not read `config.toml`. Local backup/restore uses
`JMODGUARD_CONDUCTOR_BACKUP_ROOT` and the other conductor-only keys in
[CONDUCTOR.md](CONDUCTOR.md). Guild API sessions cannot authenticate that origin.

## 6. Configure

```bash
cp config.example.toml config.toml
```

Edit `config.toml`:

```toml
[discord]
token = "PASTE_BOT_TOKEN_HERE"

[web]
base_url = "http://localhost:8080"          # must match the OAuth redirect
oauth_client_id = "PASTE_CLIENT_ID"
oauth_client_secret = "PASTE_CLIENT_SECRET"

[bot]
owner_id = 123456789012345678               # your Discord user ID
```

- **Find your user ID**: Discord → Settings → Advanced → enable **Developer
  Mode**, then right-click your avatar → **Copy User ID**.
- **`owner_id` is required for global config.** Leaving it at `0` blocks every
  WebUI “Global” edit (global prefix, global rules/commands/templates/badwords).
  The API logs a startup warning when `owner_id` is unset.
- Only the bot needs the bot token (`JMODGUARD_DISCORD_TOKEN`); the API uses OAuth.
  Deploy matching bot/API/frontend builds. Administrative changes are queued; use the
  Actions tab to refresh their status and to cancel unstarted or stuck log-channel work.
  See [action settings](BOT-ACTIONS.md#runtime-controls).

## 7. Run

```bash
./target/release/jmodguard-bot        # Discord gateway; migrates DB
./target/release/jmodguard-api        # WebUI/API on 127.0.0.1:8080
```

This local API serves the admin pages itself. On Bunny the pages live on
CDN/Storage and the API container is private — see [DEPLOY-BUNNY.md](DEPLOY-BUNNY.md).

Run the bot and API in separate terminals (or as the services below): the bot
is a long-running Discord gateway, so it does not return before the API command.

First boot (bot):
- creates `jmodguard.db` (SQLite, WAL mode) and applies migrations,
- seeds default rules, commands, and event templates (all editable later),
- syncs slash commands into every server it is in.

The API process serves the WebUI on `127.0.0.1:8080`. It **requires** Discord
OAuth client id/secret (it exits immediately if they are empty). If the bot is
still migrating, the API waits about 30 seconds locally (about six minutes
against a remote database), then serves HTTP. If that wait expires, the API
exits.

### Linux (systemd)

`/etc/systemd/system/jmodguard-bot.service`:

```ini
[Unit]
Description=jModGuard Discord bot
After=network-online.target

[Service]
User=jmodguard
WorkingDirectory=/opt/jmodguard
ExecStart=/opt/jmodguard/target/release/jmodguard-bot
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

`/etc/systemd/system/jmodguard-api.service`:

```ini
[Unit]
Description=jModGuard API / WebUI
After=network-online.target jmodguard-bot.service

[Service]
User=jmodguard
WorkingDirectory=/opt/jmodguard
ExecStart=/opt/jmodguard/target/release/jmodguard-api
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

`After=jmodguard-bot.service` only waits for the bot **process** to exist, not
for bootstrap to finish. The API retries incomplete-bootstrap, transient
SQLite-lock, and (on remote databases) connect/timeout errors so a first-boot
race does not crash-loop. Local SQLite waits about 30 seconds; remote waits
about six minutes.

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now jmodguard-bot jmodguard-api
journalctl -u jmodguard-bot -u jmodguard-api -f    # logs
```

## 8. Open the WebUI

1. Browse to `http://localhost:8080` (or your remote host).
2. **Login with Discord** → authorize.
3. Only servers where you have **Manage Server** permission appear in the
   dropdown.

> **Security:** the WebUI binds to `127.0.0.1` by default. If you expose it
> over the internet, put a reverse proxy (nginx/Caddy) in front with HTTPS,
> and consider setting `web.bind = "0.0.0.0:8080"` behind the proxy.

## 9. Recommended first-time configuration

1. **Tune automod in dry-run first.** Every rule has a dry-run toggle in the
   WebUI (Automod Rules → Edit → Dry-run). Dry-run logs what *would* happen
   without acting. Run it a few days, watch the audit log, then switch rules
   live.
2. Set up **log channels** (Log Channels tab): at minimum `automod`,
   `mod_action`, `member_join`, `member_leave`, `message_delete`. `message_edit`
   reports only Discord-identified edits (`edited_timestamp`), not GIF/link
   unfurls, chat replies, or the bot's own log posts. For each event, optionally save ignored
   user or bot IDs and source channel IDs to avoid duplicate bot reports or
   exclude staff channels.
3. Add your **badwords** (Badwords tab) and test them with the live tester.
4. Change the prefix / bot nickname if you like (Settings tab).
5. Point the slash commands at your mods and try `/ban`, `/kick`, `/warn`.

## 10. Updating

```bash
git pull
cargo build --locked --release -p jmodguard-bot
cargo build --locked --release -p jmodguard-api
# restart both services; the database and config persist
```

## 11. Troubleshooting

**The bot ignores all commands / nothing happens when I type `!about`**

Work through these in order (each one is logged, so check
`logs/jmodguard-bot.log.YYYY-MM-DD` / `logs/jmodguard-api.log.YYYY-MM-DD` first):

1. **Are the processes running?** Dual-process: check each log for its own
   boot lines.    The bot log should show `database ready (migrate=true)` and
   `starting shards (bot process; no public HTTP API)`. The API log should show
   `database ready (migrate=false)` and `API listening on http://…`.
   Looking for `WebUI listening` in the **bot** log is wrong — the bot only
   binds HTTP if `JMODGUARD_HEALTH_BIND` is set (container images: `/health` on
   8080). If a log is empty, that process never started (check the
   service/working directory — each process creates `logs/` and uses
   `jmodguard.db` relative to its *current working directory*).
2. **Privileged intents.** If you see `received messages with empty content...`
   in the log, the **Server Members** and **Message Content** intents are not
   enabled in the developer portal (Bot → Privileged Gateway Intents).
   Enable them and restart. Without Message Content the bot literally cannot
   see what you type, so every command is ignored silently.
3. **Stale binary.** The running process may predate your last build —
   restart it.
4. **Send Messages permission.** If the log shows command dispatches
   (`command "about" invoked by …`) but no reply appears, the bot lacks
   **Send Messages** in that channel (server role/override). Give it the
   permission and try again.
5. **Wrong prefix.** A message starting with `!` that is not a known command
   now gets the reply `Unknown command … Use !help …` — if you see that, your
   prefix is right and the registry is loaded.

**The log file is full of garbage characters** — older builds wrote ANSI
color codes. Rebuild with the current code (`.with_ansi(false)`); the lines
are plain `timestamp LEVEL message`.

**The WebUI audit log is empty** — the bot now writes a `system` "Bot started"
event into every server's audit log on boot, so the tab should never be empty
once the bot has started once. No events afterwards means nothing tracked
happened yet (member joins/leaves, message edits/deletes, automod hits, mod
actions are what populate it).

**Diagnostics in the log:** with the default `log.level = "info"`, the bot
logs every command invocation, unknown-prefix attempts, empty-content
warnings, and per-message debug lines (`jmodguard=debug` is always enabled
for its own crate). Set `log.level = "debug"` for full tracing.
