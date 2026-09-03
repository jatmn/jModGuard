# Research Notes

Findings recorded during development so future sessions inherit them without
re-fetching. Verified against the pinned versions in `Cargo.toml`.

## serenity 0.12.5 (June 2026)

- **`standard_framework` is deprecated**; poise is the recommended framework,
  but its static command model conflicts with a data-driven command registry.
  jModGuard therefore uses raw serenity + its own dispatch engine.
- **EventHandler broke in 0.12.5**: methods receive different argument
  shapes than 0.12.0 docs imply. Verified signatures (from
  `src/client/event_handler.rs`):
  - `guild_create(&self, ctx, guild: Guild, is_new: Option<bool>)`
  - `guild_member_addition(&self, ctx, new_member: Member)` — no guild_id
    (use `new_member.guild_id`)
  - `guild_member_update(&self, ctx, old_if_available, new, event)`
  - `message_delete(&self, ctx, channel_id, deleted_message_id, guild_id)`
  - `message(&self, ctx, new_message: Message)`
- **CacheRef is `!Send`** (holds dashmap guards). Never hold
  `cache.guild(id)` / `cache.user(id)` refs across `.await`; clone the data
  (e.g. `g.members.get(&id).cloned()`, `g.roles.clone()`) and drop the ref
  first. The `CommandInteraction` is also `!Send` (its resolved data borrows
  the cache): extract owned fields (id, token, options as strings, member
  clone) before awaiting.
- `Timestamp` is a **newtype** over `chrono::DateTime<Utc>` (chrono feature),
  not an alias: construct with `Timestamp::from_unix_timestamp(secs)`,
  format with `.to_string()`. `Message::created_at()` no longer exists —
  `Message.timestamp` is a plain `Timestamp` field.
- Cache settings: `serenity::cache::Settings` (fields `max_messages`,
  `time_to_live`, `cache_guilds`, `cache_channels`, `cache_users`). Set
  `max_messages = 0` when tracking messages in your own store.
- Slash commands: `GuildId::set_commands(http, Vec<CreateCommand>)`
  (was `set_application_commands` in older versions). Interaction response:
  `CreateInteractionResponse` + `Builder::execute` (import
  `serenity::builder::Builder`); defer via
  `CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new())`.
  `EditInteractionResponse::execute(cache_http, token: &str)` — token only.
- Renames in 0.12.5: `Interaction::ApplicationCommand` → `Interaction::Command`,
  `ApplicationCommandInteraction` → `CommandInteraction`,
  `Http::create_dm_channel` → `create_private_channel(&serde_json::Value)`,
  `ChannelId::messages` takes a `GetMessages` builder (not a closure),
  `EditMember::nickname(impl Into<String>)` (no `Some`), `EditChannel::rate_limit_per_user(u16)`.
- Interaction model modules are private; import types from
  `serenity::model::application::*` (they are re-exported there).
- Intents used: `GUILD_MEMBERS | GUILD_MESSAGES | MESSAGE_CONTENT | GUILD_MODERATION`.
  GUILD_MEMBERS and MESSAGE_CONTENT are privileged (enable in portal).

## libsql 0.9.30

- Bunny Database uses the remote libSQL protocol. Build it with
  `Builder::new_remote(url, token)` plus the in-tree rustls 0.23 connector
  (`src/remote_https.rs`); local installs use `Builder::new_local`.
  Do not enable libsql's `tls` feature: it pulls `hyper-rustls 0.25` /
  `rustls 0.22` / `rustls-webpki 0.102.8` (GHSA-82j2-j2ch-gfr8).
- `remote` still pulls `libsql-sqlite3-parser` 0.13.x. crates.io never
  published a fix for GHSA-8m95-fffc-h4c5 (CVE-2025-47736: Debug panic on
  invalid UTF-8). Git-patch it to branch `vendor/libsql-sqlite3-parser-0.13.1`
  (0.13.0 sources + lemon-rs `from_utf8_lossy`, commit `f2391d6`).
  CI and Docker fetch that pin with `GITHUB_TOKEN` only for `cargo fetch`
  (`CARGO_NET_GIT_FETCH_WITH_CLI`; Docker BuildKit secret `github_token`).
  Compile/test/image `cargo build` runs after fetch, without the token in
  gitconfig, env, or the secret mount. Local images:
  `docker build --secret id=github_token,env=GITHUB_TOKEN -f Dockerfile.bot .`
  Local `cargo` uses git(1) via `.cargo/config.toml` so clone credentials work.
- The libSQL API is async. `src/db_compat.rs` supplies a narrow synchronous
  facade so the existing SQL paths can run unchanged inside `with_conn`'s
  blocking closures.
- A Tokio runtime must not be dropped from an async context. The compatibility
  runtime is therefore owned and dropped by a dedicated OS thread.
- `with_conn` closures need `Send + 'static` (spawn_blocking): convert all
  borrowed `&str` params to owned `String` before the closure.
- Generic `load_rows` needs `T: Send + 'static`.
- Closures in separate `if` branches get distinct types — use a single
  statement + conditional SQL string instead.

## reqwest 0.13

- `default-features = false` needs explicit features: `json`, `form`, `rustls`,
  `http2`, `charset`. (No `rustls-tls` feature name in 0.13.)

## axum 0.8

- `FromRef` derive needs the **macros** feature; here we pass
  `Arc<AppState>` as state directly, so it is unused.
- Path params use `{name}` syntax: `"/api/guilds/{guild_id}/config"`.
- tower-sessions 0.14 has no `cookie-store` feature (memory-store only);
  `Session::remove` is generic (`let _: Option<String> = …`).

## Scaling / memory-safety decisions

- All automod state is per-(guild,user) dashmap buckets pruned every 60s;
  lookups are O(1) per message, never full-map scans.
- Serenity message cache disabled (own SQLite store with retention).
- DB operations are limited by `db.pool_size` in each process. Local installs
  use SQLite WAL; Bunny deployments use remote libSQL. There are no unbounded
  queues. If a future raid causes write contention, batch message inserts via
  a writer task with a bounded channel.
- OAuth guild/user data cached in-memory for 60s to limit Discord API calls.

## Production bugfixes (2026-08-05)

- **`/users/@me/guilds` returns `permissions` as an integer**, despite the
  Discord docs saying string. Deserialize with an untagged
  `PermissionsField` enum accepting both (`src/state.rs`).
- **Empty log file**: the `tracing_appender::non_blocking` guard must outlive
  the process. Dropping it at the end of `init_tracing` killed the writer
  thread and silently lost every log line. Fix: `std::mem::forget(guard)`.
- **Startup "database is locked"**: connections racing the migration
  transaction. Fix: migrate + seed globals on a dedicated bootstrap connection
  before normal database traffic (`Db::open` in `src/db.rs`).
- **Bot silently ignored ALL commands**: `load_config_parts` used a
  hardcoded `data` value column for every table, but `templates` stores its
  value in `template` and `log_channels` in `channel_id`. Every config load
  failed → the message handler bailed at `message: config load failed` before
  command dispatch. Fix: schema-aware mappers `rows_map_json` /
  `rows_map_plain` in `src/db.rs` + regression test that loads all four
  sections for a real guild. Symptom in logs:
  `no such column: data in SELECT event_type, data FROM templates`. This is
  why the per-message diagnostics were added (handlers.rs, commands/mod.rs).
