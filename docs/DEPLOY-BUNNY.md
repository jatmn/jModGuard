# Deploy on Bunny.net

jModGuard's remote backend uses the standard libSQL protocol rather than a
provider-specific data API. [Bunny Database](https://docs.bunny.net/database)
is the first documented, first-class remote provider and works out of the box
with Bunny's standard injected credentials. Automated tests cover those
environment names and the generic libSQL protocol; the acceptance steps below
cover Bunny's production TLS/authentication service. The bot and API run as
two independent Magic Container apps and use the database as their shared source of truth.
This is the intended Bunny topology; no shared disk or existing-data migration
is needed for a first deployment.

```text
Discord Gateway                         Public HTTPS endpoint
       |                                         |
       v                                         v
jmodguard-bot app                         jmodguard-api app
       |                                         |
       +---------- Bunny Database ---------------+
                  (remote libSQL)
```

The API embeds `frontend/`, so an initial deployment does not require a Storage
Zone, Pull Zone, or cross-origin CORS configuration.

## 1. Create the database

Create a Bunny Database in the dashboard or with the Bunny CLI. In the
database's **Access** page, generate full-access credentials for each app.
Both apps need write access: the bot writes moderation state and migrations,
while the API writes configuration changes.

Bunny can inject these standard environment variables into a Magic Container
app:

```dotenv
BUNNY_DATABASE_URL=libsql://DATABASE_ID.lite.bunnydb.net
BUNNY_DATABASE_AUTH_TOKEN=SECRET
```

jModGuard reads those names directly. Its portable aliases
`JMODGUARD_DB_URL` and `JMODGUARD_DB_TOKEN` are also supported and take
precedence when both forms are present. Never commit an access token.

## 2. Build and publish both images

Build both images from the same reviewed commit or release tag:

```bash
docker build -f Dockerfile.bot -t REGISTRY/jmodguard-bot:TAG .
docker build -f Dockerfile.api -t REGISTRY/jmodguard-api:TAG .
docker push REGISTRY/jmodguard-bot:TAG
docker push REGISTRY/jmodguard-api:TAG
```

Record immutable image digests in the private ops repository. Do not deploy
floating `main` or `latest` tags.

The images default `JMODGUARD_LOG_DIRECTORY` to an empty value so logs go to
stdout/stderr and appear in the Magic Containers log stream. A persistent
volume is not required for the remote database topology.

## 3. Create the bot app

Create a Magic Container app from the bot image. Start with one instance. Add
the database credentials from step 1 and these secrets/settings:

```dotenv
JMODGUARD_DISCORD_TOKEN=SECRET
JMODGUARD_OWNER_ID=123456789012345678
JMODGUARD_CONFIG_POLL_SECS=5
JMODGUARD_LOG_LEVEL=info
JMODGUARD_LOG_DIRECTORY=
```

The bot is the schema authority. On the first start it creates the schema,
seeds defaults, and publishes a durable bootstrap marker before connecting to
Discord. This is initial schema creation on an empty service, not a migration
of an older deployment.

Wait for logs containing `database ready (migrate=true)` before starting the
API app. A fake or invalid Discord token can still be used for a database-only
smoke test; Discord will then reject the later gateway connection as expected.

## 4. Create the API app

Create a separate Magic Container app from the API image. Attach credentials
for the same Bunny Database and configure:

```dotenv
JMODGUARD_DISCORD_TOKEN=SECRET
JMODGUARD_WEB_BIND=0.0.0.0:8080
JMODGUARD_WEB_BASE_URL=https://YOUR-API-HOSTNAME
JMODGUARD_OAUTH_CLIENT_ID=SECRET
JMODGUARD_OAUTH_CLIENT_SECRET=SECRET
JMODGUARD_OWNER_ID=123456789012345678
JMODGUARD_CONFIG_POLL_SECS=5
JMODGUARD_LOG_LEVEL=info
JMODGUARD_LOG_DIRECTORY=
```

The API also needs the Discord bot token because WebUI unban uses Discord's
REST API. Add a public HTTPS endpoint for container port `8080`. In the Discord
Developer Portal, set the OAuth redirect URI to this exact value:

```text
https://YOUR-API-HOSTNAME/oauth/callback
```

The API never applies migrations. It verifies the schema and bot bootstrap
marker, retrying briefly if it starts while the bot is still bootstrapping.

## 5. Acceptance test

For the first deployment:

1. Confirm bot logs show `database ready (migrate=true)` and then shard startup.
2. Confirm API logs show `database ready (migrate=false)` and
   `API/WebUI listening`.
3. Request `/`; it must return the embedded jModGuard UI over HTTPS.
4. Complete Discord OAuth login and open a guild where the user has Manage
   Server permission.
5. Change a harmless guild prefix in the UI. Confirm the bot eventually
   observes it after polling and any replica convergence, exercise the new
   prefix in Discord, then restore it.
6. Configure a dry-run moderation rule and verify its audit event.
7. Restart each app independently and confirm both reconnect to the same data.

The repository's CI also runs `tests/remote_database_smoke.rs` against a pinned
libSQL server. That test proves bot bootstrap, a remote write, API schema-open,
and visibility across independent database handles without requiring
production credentials. `tests/dual_process_smoke.rs` separately exercises
the bot/API process contract against local SQLite.

## Operating constraints

- Bunny Database is currently documented as Public Preview. Review its current
  limits and availability before production use. Its internal snapshots are
  not a substitute for an operator-controlled backup policy. The current
  `bsql .dump` output is not a complete jModGuard backup; review the explicit
  production blocker in [BACKUP-RESTORE.md](BACKUP-RESTORE.md).
- Run one bot app instance unless Discord sharding and singleton work have been
  deliberately redesigned and tested.
- API sessions use an in-memory store. Restarting the API logs users out, and
  multiple API replicas do not share sessions.
- Both processes poll durable `config_version`; command changes additionally
  bump `slash_registry_version` for Discord slash-command resync.
- Keep bot and API on the exact same source/image version during rollout.

## Optional separate SPA

The embedded same-origin UI is the safest starting point. If `frontend/` is
later hosted on Bunny Storage/CDN under another origin:

1. Upload the frontend from the exact deployed source tag.
2. Set `window.JMODGUARD_API_BASE` to the API origin.
3. Set `JMODGUARD_CORS_ORIGINS` to the exact HTTPS SPA origin; never use `*`.
4. Keep `JMODGUARD_WEB_BASE_URL` on HTTPS so cross-origin session cookies use
   `Secure` and `SameSite=None`.
5. Keep the Discord callback at `{web.base_url}/oauth/callback`.

## References

- [Bunny Database overview](https://docs.bunny.net/database)
- [Bunny Database Rust client](https://docs.bunny.net/database/connect/rust)
- [Bunny Database authentication](https://docs.bunny.net/database/connect/authorization)
- [Magic Containers quickstart](https://docs.bunny.net/magic-containers/quickstart)
- [Backup and restore status](BACKUP-RESTORE.md)
- [Configuration reference](CONFIGURATION.md)
- [Architecture](ARCHITECTURE.md)
