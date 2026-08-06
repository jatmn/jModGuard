# jModGuard — Decoupled All-Bunny Deployment Plan

## 1. Goal

Deploy jModGuard as a decoupled, all-Bunny.net stack: the Discord bot (autopilot daemon) and the admin WebUI live in **separate containers**, sharing a **managed Bunny Database**. No persistent volumes, no third-party vendors. Target: ~$2.50-4.50/mo.

## 2. Locked decisions

| Decision | Choice |
|---|---|
| Bot host | Bunny **Magic Containers** (always-on, pay-as-you-go) |
| WebUI | **Fully decoupled** from the bot process |
| Containers | **2 Magic Container apps** (bot + API; separate pods for independent restarts) |
| Database | **Bunny Database** (managed libSQL/SQLite; both clients write the same primary) |
| Frontend | Static SPA on **Bunny Storage + CDN** |
| DNS/TLS | **Bunny DNS + CDN** (free Let's Encrypt SSL) |
| API runtime | **Rust axum container** (reuses existing `web/` code) |
| Cache invalidation | `config_version` poll in DB (bot polls every 5-10s) |
| Volumes | None — DB is network-hosted |
| Public exposure | **Only** `admin.yourdomain.com` (CDN → SPA + API). Bot has **no** MC endpoints |
| DreamCompute / VPS / AWS | Rejected (kept for reference only) |

## 3. Architecture

```
Internet
   │
   ▼
admin.yourdomain.com  (ONLY public entry)
   ├─ Bunny DNS + CDN (TLS)
   ├─ SPA  ← Bunny Storage
   └─ /api/* /oauth/*  →  API Magic Container  (CDN endpoint only; no Anycast)
            │                    ▲
            │                    │  auth token (server-side)
            ▼                    │
      Bunny Database ◄───────────┴── Bot Magic Container (NO public endpoints;
                                      outbound only: Discord gateway + DB)
```

- The **bot never calls the API**. Both processes talk to the DB directly; the DB is the single source of truth.
- Bot commands and WebUI edits both bump `config_version`; the bot's per-guild cache (state.rs) refreshes within the poll window.

## 3a. Network exposure & security (locked posture)

Goal: nothing reachable from the public internet except the admin WebUI hostname. Deploy/registry access stays via Bunny dashboard / image pull — not a runtime entry for users.

| Surface | Public? | How |
|---|---|---|
| **Bot** MC app | **No** | Create **zero** Endpoints (CDN or Anycast). Bunny docs: endpoints are optional; without them the app is not internet-reachable. Bot only needs **outbound** Discord + DB. |
| **API** MC app | **Only via CDN hostname** | One **CDN** endpoint as origin for `admin.yourdomain.com` (`/api/*`, `/oauth/*`). **No Anycast IP** ($2/mo and direct IP exposure). Do not publish/use any raw `*.bunny.run` (or similar) URL — custom domain only. |
| **Bunny Database** | **Not a public app**, but also **not a private VPC** | Reachable at `libsql://….lite.bunnydb.net` with an **access token**. Anyone with URL+token can connect from anywhere. Security = token secrecy, not network isolation. Tokens only in MC secrets (`BUNNY_DATABASE_*` / `JMODGUARD_DB_*`); **never** in the browser or SPA. |
| **SPA / Storage** | Yes (static only) | No secrets in frontend. Relative `/api` calls same-origin through CDN. |
| **Deploy / updates** | Operator-only | Image push to registry + Bunny MC redeploy / dashboard. Not a user-facing runtime port. |

Hard rules:

1. **Bot app: no Endpoints.** Confirm after deploy that no CDN/Anycast row exists on that app.
2. **API app: CDN endpoint only** — never Anycast. Prefer binding the CDN pull zone so the only DNS name that matters is `admin.yourdomain.com`.
3. **All API routes require Discord OAuth session** (already the model); reject unauthenticated `/api/*`. Secure cookies (`Secure`, tight SameSite) behind HTTPS.
4. **DB tokens** full-access only on bot+api containers; regenerate if leaked; never ship token to clients.
5. Optional hardening (verify in Bunny console at deploy): Bunny Shield / WAF / rate limits on the admin pull zone; deny listing any accidental second hostname.

Honest limit: Bunny does **not** document a customer VPC that makes Database or MC origins unreachable except from other Bunny apps. The achievable posture is **no bot endpoints + token-gated DB + single CDN-fronted admin API** — not “air-gapped private network.”

## 3b. Current code vs what must be built (detether plan)

Today is a **single binary**: `src/main.rs` opens SQLite, spawns the axum WebUI, then starts the Discord gateway. Almost everything below is still coupled in-process.

| Area | Exists today | Must build / change |
|---|---|---|
| Process split | One `jmodguard` binary runs bot **and** WebUI | Separate `bot` and `api` binaries; bot has **no** HTTP; API has **no** serenity gateway |
| Crate layout | Flat `src/` (`db.rs`, `types.rs`, `web/`, `commands/`, …) | Cargo workspace: `bot/`, `api/`, `db/`, `types/`, `frontend/` (Phase 1 tree) |
| Shared state | One `AppState`: automod + OAuth + config cache + Discord caches | Split: bot state (automod, config cache, cooldowns) vs API state (OAuth, session, guild/user caches). Stop sharing one mega-struct |
| Config | One `Config` with `[discord]+[db]+[web]+[bot]+[log]` | Bot-only vs API-only config/env; drop Discord token from API; drop OAuth from bot; add `db.url`/`db.token` (and poll interval) |
| Config invalidation | In-process `invalidate_configs()` (`AtomicU64` + 15s TTL) after WebUI/command writes | Durable DB `config_version`; bump on **config** writes only; bot **poll loop** (5–10s) to reload cache across processes |
| DB engine | `rusqlite` + `r2d2` file path (`JMODGUARD_DB_PATH`) | Shared `db` crate on **libsql** SDK; local file for dev/tests; remote URL+token for Bunny; skip file-only PRAGMAs on remote |
| Migrations | Bot/process startup migrates via `PRAGMA user_version` | Keep migrate-on-bot (or one-shot job); API opens read/write **without** migrating and fails if schema missing |
| SPA | Embedded in API via `include_str!("static/…")` in `web/mod.rs` | Move to `frontend/`; API is JSON(+OAuth) only in containers; CDN serves static; SPA may need absolute API base if not same-origin |
| CORS / cookies | Local same-origin; `MemoryStore` sessions; `secure=false` | Production: CDN same-origin to API **or** CORS + cookie settings; `secure=true` behind HTTPS; decide on session secret / store if API replicas > 1 |
| Docker | None | `Dockerfile.bot` / `Dockerfile.api` (bookworm multi-stage) |
| Ops extras | Local `logs/` dir, file DB | Stdout logging in containers; no app volumes; backup export job (Phase 7) |

**Reuse as-is (move, don’t rewrite):** automod, handlers, commands/plans/behaviors, logging, templates, normalize, defaults seeding, REST handlers in `web/api.rs`, OAuth helpers in `state.rs`, schema in `migrations/0001_init.sql`.

**Suggested build order (source, before Bunny deploy):**
1. Workspace split + two binaries (still on local rusqlite) — prove bot-only / API-only against one DB file.
2. `config_version` migration + bump in DB writers + bot poll (replace cross-process reliance on `invalidate_configs`).
3. Detether SPA to `frontend/`; API stops embedding static in container builds.
4. Swap `db` crate to libsql (local + remote URL/token); fail closed if remote misconfigured.
5. Dockerfiles + env split; only then Phases 4–8 on Bunny.

## 4. Components & responsibilities

| Component | Runs | Talks to | Carries |
|---|---|---|---|
| `bot` container | serenity gateway, automod, prefix/slash commands, DB writes (evidence, member tracking, config edits) | Bunny Database, Discord | `JMODGUARD_DISCORD_TOKEN`, DB creds, `owner_id` |
| `api` container | axum: OAuth login, JSON CRUD for rules/commands/badwords/templates/guild config, bumps `config_version` | Bunny Database, Discord OAuth, browser (CORS) | DB creds, OAuth client id/secret, `base_url` |
| SPA | static files on Storage, served via CDN at `admin.yourdomain.com` | API only | nothing |

## 5. Repo restructure (Phase 1)

Convert jModGuard to a cargo workspace (see §3b for what must leave the monolith):

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

- Split `AppState` / `Config` per binary (bot vs API). Local DX: run both against the same DB file.
- Migrations run once, at bot startup. API never migrates (requires schema already present).
- Web/OAuth config (`JMODGUARD_WEB_*`, `JMODGUARD_OAUTH_*`) moves to the API; bot keeps token/log/owner/db settings.
- Detether SPA from axum embeds into `frontend/`.

## 6. Database migration (Phase 2)

- Swap `rusqlite`/`r2d2_sqlite` → **libsql Rust SDK** in the shared `db` crate.
- **Schema and `migrations/` unchanged** (SQLite dialect — libSQL compatible).
- Add `config_version` marker; config edit paths bump it (not runtime message/member writes); bot polls and reloads changed guild configs every 5-10s.
- Migrate existing local `jmodguard.db` → export and import into Bunny Database once (via libSQL/SQLite tools).

## 7. Containers (Phase 3)

- `Dockerfile.bot`: current stable `rust:bookworm` builder → `debian:bookworm-slim` + `ca-certificates`; entrypoint = bot daemon.
- `Dockerfile.api`: same base; entrypoint = axum API (JSON only, no static serving).
- Push images to Docker Hub (or Bunny-supported registry — verify).

## 8. Bunny console setup (Phase 4)

1. **Bunny Database**: create database → note URL + access token (access token stays server-side only — never in the browser).
2. **Bunny Storage**: create bucket → upload `frontend/` build output.
3. **Bunny DNS**: add `yourdomain.com` (or subdomain) as a DNS zone.
4. **Bunny CDN**: pull zone + free SSL; SPA from Storage; origin-route `/api/*` and `/oauth/*` to the API app (cache-bypass); SPA fallback for the rest.
5. **Magic Containers**: two apps (`bot`, `api`). **Bot: no Endpoints.** **API: CDN endpoint only (no Anycast).** Wire CDN origin for `/api/*` + `/oauth/*`. Inject DB secrets via dashboard (“Add Secrets to Magic Container App”); never bake tokens into images.

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

Source (before deploy):

- [ ] `cargo test` / clippy on workspace
- [ ] Bot-only and API-only binaries against one local DB
- [ ] API config edit bumps `config_version`; bot cache refreshes within poll window
- [ ] Bot command config edit visible in WebUI/API
- [ ] API refuses unmigrated DB; bot migrates once

Deploy:

- [ ] Bot connects to gateway; slash commands sync
- [ ] WebUI login via Discord OAuth works at `https://admin.yourdomain.com`
- [ ] WebUI config edit propagates to bot within poll window (≤10s)
- [ ] Bot command config edit visible in WebUI
- [ ] Restarting the API app does not disconnect the bot (and vice versa)
- [ ] Migrations ran once; no lock contention in logs
- [ ] Bot MC app has **zero** public endpoints (cannot hit it from the internet)
- [ ] API not reachable via Anycast IP; admin works only on `admin.yourdomain.com`
- [ ] DB token not present in SPA/browser; unauthenticated `/api/*` rejected

## 13. Costs (steady state, 50 msg/min)

| Item | $/mo |
|---|---|
| Bot container (RAM/CPU) | ~$1.30-2.30 |
| API container (RAM/CPU) | ~$0.50-1.50 |
| SPA (Storage + CDN) | ~$0-1 |
| Bunny Database (writes ~2.16M rows/mo, storage) | $0 (preview) → ~$0.65-1.40 |
| Egress (Discord + admin UI) | ~$0.02-0.05 |
| Inter-service traffic (bot↔api↔DB) | **$0** |
| **Total** | **~$2.50-4.50** estimate (+ domain ~$10-15/yr, optional); gateway CPU can push higher |

Bunny billing model: internal movement is free; you pay for compute (RAM/CPU — $0.02/core-hr, $0.005/GB-hr), DB rows read/written + storage, and egress leaving the platform ($0.01/GB EU/NA; CDN-routed traffic bills under CDN).

## 14. Verify at deploy (open items)

- [ ] MC default hostname format + TLS behavior when the app is created
- [ ] Whether the CDN tier carries a ~$1/mo minimum
- [ ] In-memory API sessions (`MemoryStore`) vs shared store if API scales past one instance
- [ ] Bunny Database backup/export tooling during preview
- [ ] Container registry options for image push
- [ ] Confirm no auto-created endpoints on Quick Deploy for the bot image
- [ ] If multi-tenant SaaS: raise Bunny DB count/size limits with support before GA traffic

## 15. Multi-tenancy & database strategy (research)

Question: one shared DB vs **database-per-Discord-server** vs **database-per-administrator**, for security when multiple people use the bot on their own server(s).

### Bunny Database constraints (current docs)

| Constraint | Value | Source |
|---|---|---|
| Max databases / account | **50** (preview; raisable via support) | [limits](https://bunny.net/docs/database/limits) |
| Max size / database | **1 GB** (preview; raisable) | same |
| Writer model | **Single writer per database** (SQLite/libSQL) | Bunny product notes |
| Auth | URL + access token **per database** (full or read-only) | [authorization](https://docs.bunny.net/database/connect/authorization) |
| Pricing (post-preview) | $0.30 / B reads, $0.30 / M writes, **$0.10 / GB / active region / mo** storage | [pricing](https://bunny.net/database/) |
| Preview | Free | same |

There is **no** documented private VPC that isolates a DB to only your MC apps. Isolation options are: (1) application checks, (2) separate databases + separate tokens, (3) token hygiene.

### What the code already does

Schema is already **multi-guild in one DB**: almost every table is keyed by `guild_id`, with `guild_id = 0` as global defaults. WebUI already gates by Discord OAuth + `MANAGE_GUILD`. One bot process + one DB can host many servers today.

### Options

| Model | Isolation | Fit | Cost / ops |
|---|---|---|---|
| **A. One shared DB** (recommended default) | Soft: every query must filter `guild_id`; bug = cross-tenant leak risk | Matches current design; best for “one bot install, many guilds” or a single operator | **Lowest**: one storage footprint, one token pair, one migrate path, one `config_version` |
| **B. DB per administrator** (SaaS tenant) | Hard: customer A’s token cannot open customer B’s DB | Use when selling the bot to many admins who must not share blast radius | **Medium+**: control-plane map `admin → {url,token}`; bot/api route by tenant; N migrates; preview **≤50 admins** unless limit raised. Storage ≈ N × (min primary region footprint) even if each DB is tiny |
| **C. DB per Discord server** | Hardest isolation | Rarely worth it; only compliance/“nuke one guild’s data by dropping DB” | **Highest ops**: one guild ≈ one DB → **50 guilds max** in preview; bot needs many connections / dynamic open; per-guild `config_version`; connection churn. Row/write $ still ≈ same traffic, but **N storage bills** and control-plane complexity |

### Best practice recommendation

1. **Stay on model A** for the decoupled Bunny deploy in this doc: one Bunny Database, `guild_id` tenancy, OAuth guild checks, DB token only in bot+api secrets.
2. Harden A (do these either way): never interpolate guild ids from the client without auth; keep all SQL scoped by authorized `guild_id`; no shared `GLOBAL_SCOPE` writes from non-owner paths if you later multi-tenant commercially; automated tests for “guild B cannot read guild A”.
3. **Consider model B only** if this becomes a multi-customer product and soft isolation is unacceptable. Then add a small **control-plane** DB (or table) of tenants → Bunny DB credentials; provision a DB when an admin onboards; bot selects pool by guild→tenant map. Still **not** one DB per guild.
4. **Defer model C** unless a customer pays for dedicated isolation. It fights Bunny’s 50-DB preview cap and the single-bot connection model.

### Added cost (order of magnitude)

- **A → same traffic on B/C:** row read/write charges are about the **same** (usage-based, not “per database fee”).
- **Extra for B/C:** storage **per database per active region** ($0.10/GB-mo). Empty-ish tenant DBs still occupy a primary region when active; idle DBs can spin down to object storage (Bunny: idle moves off hot regions) but you still operate N databases.
- **Example:** 20 admin-tenants (B) ≈ up to ~20× storage floor vs one DB, plus provisioning/migrate/token ops — not 20× row costs if total messages are unchanged.
- **Preview:** multi-DB is free but **capped at 50**.

### Planning stance

- **This deploy:** model **A** (single DB). Section 15 previously waved at “per-server databases” as deferred — keep them deferred; prefer B over C if ever needed.
- **Open product decision (not required to ship the Bunny split):** whether jModGuard is single-operator multi-guild (A) or multi-customer SaaS (B). That choice drives control-plane work, not the bot/api process split.

## 16. Deferred (explicitly not now)

Postgres port · Edge Scripting as primary compute · **database-per-server (model C)** · AWS/ECS · Cloudflare D1 — evaluated; not needed for the first decoupled Bunny path. **Database-per-admin (model B)** only if product direction becomes multi-customer SaaS.
