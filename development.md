# jModGuard — Decoupled All-Bunny Deployment Plan

## 1. Goal

Deploy jModGuard as a decoupled, all-Bunny.net stack: the Discord bot (autopilot daemon) and the admin WebUI live in **separate containers**, sharing a **managed Bunny Database**. No persistent volumes, no third-party vendors. Target: ~$2.50-4.50/mo.

## 2. Locked decisions

| Decision | Choice |
|---|---|
| Bot host | Bunny **Magic Containers** (always-on, pay-as-you-go) |
| WebUI | **Fully decoupled** from the bot process |
| Containers | **2 containers** (bot + API) in one Magic Containers app |
| Database | **Bunny Database** (managed libSQL/SQLite, multi-writer) |
| Frontend | Static SPA on **Bunny Storage + CDN** |
| DNS/TLS | **Bunny DNS + CDN** (free Let's Encrypt SSL) |
| API runtime | **Rust axum container** (reuses existing `web/` code) |
| Cache invalidation | `config_version` poll in DB (bot polls every 5-10s) |
| Volumes | None — DB is network-hosted |
| DreamCompute / VPS / AWS | Rejected (kept for reference only) |

## 3. Architecture

```
admin.yourdomain.com
   ├─ Bunny DNS + CDN (free SSL, caches SPA static assets)
   ├─ SPA (static → Bunny Storage)
   └─ API container (Rust axum: Discord OAuth + JSON CRUD, bumps config_version)
            │
            ▼
      Bunny Database (libSQL — multi-writer; no traffic charges)
            ▲
   Bot container (daemon: serenity gateway, automod, commands; polls config_version)
```

- The **bot never calls the API**. Both processes talk to the DB directly; the DB is the single source of truth.
- Bot commands and WebUI edits both bump `config_version`; the bot's per-guild cache (state.rs) refreshes within the poll window.

## 4. Components & responsibilities

| Component | Runs | Talks to | Carries |
|---|---|---|---|
| `bot` container | serenity gateway, automod, prefix/slash commands, DB writes (evidence, member tracking, config edits) | Bunny Database, Discord | `DISCORD_BOT_TOKEN`, DB creds, `owner_id` |
| `api` container | axum: OAuth login, JSON CRUD for rules/commands/badwords/templates/guild config, bumps `config_version` | Bunny Database, Discord OAuth, browser (CORS) | DB creds, OAuth client id/secret, `base_url` |
| SPA | static files on Storage, served via CDN at `admin.yourdomain.com` | API only | nothing |

## 5. Repo restructure (Phase 1)

Convert jModGuard to a cargo workspace:

```
jModGuard/
├── bot/            # daemon binary: serenity, automod, commands (no HTTP)
├── api/            # axum binary: OAuth + JSON API (extracted from web/)
├── db/             # shared crate: libsql client + data layer (from db.rs)
├── types/          # shared serde models (from types.rs)
├── frontend/       # SPA assets (extracted from web/), built & uploaded to Storage
├── migrations/     # unchanged (SQLite dialect)
└── Dockerfile.bot / Dockerfile.api   # multi-stage Rust builds, slim runtime
```

- Migrations run once, at bot startup. API never migrates.
- `JMODGUARD_WEB_*` config moves to the API; bot keeps token/log/owner/db settings.

## 6. Database migration (Phase 2)

- Swap `rusqlite`/`r2d2_sqlite` → **libsql Rust SDK** in the shared `db` crate.
- **Schema and `migrations/` unchanged** (SQLite dialect — libSQL compatible).
- Add `config_version` marker; both edit paths bump it; bot polls and reloads changed guild configs every 5-10s.
- Migrate existing local `jmodguard.db` → export and import into Bunny Database once (via libSQL/SQLite tools).

## 7. Containers (Phase 3)

- `Dockerfile.bot`: `rust:1.85-alpine` builder → slim runtime + `ca-certificates`; entrypoint = bot daemon.
- `Dockerfile.api`: same base; entrypoint = axum API (JSON only, no static serving).
- Push images to Docker Hub (or Bunny-supported registry — verify).

## 8. Bunny console setup (Phase 4)

1. **Bunny Database**: create database → note URL + access token (access token stays server-side only — never in the browser).
2. **Bunny Storage**: create bucket → upload `frontend/` build output.
3. **Bunny DNS**: add `yourdomain.com` (or subdomain) as a DNS zone.
4. **Bunny CDN**: pull zone in front of Storage (SPA) + free SSL; configure SPA fallback (Edge Scripting middleware) and cache-bypass for `/api/*` and `/oauth/*`.
5. **Magic Containers**: one app, two containers (`bot`, `api`), separate env/secrets per container; expose HTTP(S) endpoint (included; Anycast IP not needed — HTTP only).

## 9. Env / secrets (Phase 5)

| Container | Env vars |
|---|---|
| `bot` | `JMODGUARD_DISCORD_TOKEN`, `JMODGUARD_OWNER_ID`, `JMODGUARD_LOG_LEVEL`, `JMODGUARD_DB_URL`, `JMODGUARD_DB_TOKEN` |
| `api` | `JMODGUARD_DB_URL`, `JMODGUARD_DB_TOKEN`, `JMODGUARD_OAUTH_CLIENT_ID`, `JMODGUARD_OAUTH_CLIENT_SECRET`, `JMODGUARD_WEB_BASE_URL=https://admin.yourdomain.com`, session secret |

- Discord token never in API env; OAuth secrets never in bot env.

## 10. Discord portal (Phase 6)

- Register redirect URI: `https://admin.yourdomain.com/oauth/callback` (must match `base_url` exactly).
- Privileged intents (Server Members, Message Content) already required — unchanged.

## 11. Backups (Phase 7)

- Bunny Database backup story is undocumented during public preview → nightly **export to Bunny Storage** via a cron'd script (container or scheduled task). ~$0.02/mo.

## 12. Verification (Phase 8)

- [ ] Bot connects to gateway; slash commands sync
- [ ] WebUI login via Discord OAuth works at `https://admin.yourdomain.com`
- [ ] WebUI config edit propagates to bot within poll window (≤10s)
- [ ] Bot command config edit visible in WebUI
- [ ] Restarting the API does not disconnect the bot (and vice versa)
- [ ] Migrations ran once; no lock contention in logs

## 13. Costs (steady state, 50 msg/min)

| Item | $/mo |
|---|---|
| Bot container (RAM/CPU) | ~$1.30-2.30 |
| API container (RAM/CPU) | ~$0.50-1.50 |
| SPA (Storage + CDN) | ~$0-1 |
| Bunny Database (writes ~2.16M rows/mo, storage) | $0 (preview) → ~$0.65-1.40 |
| Egress (Discord + admin UI) | ~$0.02-0.05 |
| Inter-service traffic (bot↔api↔DB) | **$0** |
| **Total** | **~$2.50-4.50** (+ domain ~$10-15/yr, optional) |

Bunny billing model: internal movement is free; you pay for compute (RAM/CPU per container), DB rows read/written + storage, and egress leaving the platform ($0.01/GB EU/NA; CDN-routed traffic bills under CDN).

## 14. Verify at deploy (open items)

- [ ] MC default hostname format + TLS behavior when the app is created
- [ ] Whether the CDN tier carries a ~$1/mo minimum
- [ ] Inter-container traffic within an MC app is fully internal (their "communicate locally" wording supports it)
- [ ] libSQL access-token scope options (per-database granularity)
- [ ] Bunny Database backup/export tooling during preview
- [ ] Container registry options for image push

## 15. Deferred (explicitly not now)

Postgres port · Edge Scripting API · per-server databases · AWS/ECS · Cloudflare D1 — all evaluated; none needed at current scale. Each remains a clean future path.
