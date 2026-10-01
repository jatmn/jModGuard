# Public installation

Install from **this public repository** without the private ops repo; private
operator automation is not required. The license is **PolyForm Noncommercial 1.0.0**
(`LICENSE.md`). Bunny.net remains the first-class cloud path.

## Choose a topology

| Path | Guide | Services and storage |
|---|---|---|
| Bunny | [DEPLOY-BUNNY.md](DEPLOY-BUNNY.md) | CDN WebUI, private bot/API Magic Containers, Bunny Database / libSQL |
| Local Linux | [SETUP.md](SETUP.md) | Bot and API with local SQLite or compatible libSQL |
| AWS | [DEPLOY-AWS.md](DEPLOY-AWS.md) | EC2 with SQLite or compatible libSQL; not RDS/Postgres |
| Cost | [COST.md](COST.md) | Operator measurements and current provider list prices |

## Public artifacts

Use a source commit or verified image digest:

- Source: `https://github.com/jatmn/jModGuard`
- Images: `ghcr.io/jatmn/jmodguard-bot`, `ghcr.io/jatmn/jmodguard-api`, `ghcr.io/jatmn/jmodguard-webui`
- WebUI files: `frontend/index.html`, `frontend/app.js`, `frontend/style.css`,
  `frontend/administration/index.html`, `frontend/administration.js`

Buildkite publishes changed images on main. Manual commit-SHA builds publish all
eligible components; WebUI is skipped for source revisions predating that package.
Stable tags and scheduled Nightlies use the image channels in [RELEASES.md](RELEASES.md).
Buildkite does not upload WebUI files to Bunny Storage. Publish from source or the
WebUI image using the [WebUI guide](WEBUI.md). The browser also fetches daisyUI CSS
and Headless UI from a CDN; these are not extra repository files.

## Setup

1. Follow [SETUP.md](SETUP.md) for Discord credentials and process configuration.
   Keep bot tokens on the bot and dashboard OAuth credentials on the API.
2. Create a fresh core database or use the recognized `jmodguard-core-1` family.
   Never adopt an unmarked old database; see [DATABASE-BASELINE.md](DATABASE-BASELINE.md).
3. Configure a separate guild store. Never point guild data at the core database
   or public frontend storage. Keep credentials out of git.
4. Start the bot to apply migrations, then start the API. For a source checkout:

   ```bash
   cargo run --locked -p jmodguard-bot
   # In a separate terminal after the bot initializes the database:
   cargo run --locked -p jmodguard-api
   ```

5. Open the WebUI and log in as the configured bot owner. **Administration** uses
   the existing session. For Bunny image updates and required launcher/provider
   configuration, follow [BUNNY-UPDATES.md](BUNNY-UPDATES.md).
6. Verify an operator-controlled backup and restore procedure before production.
   The application does not provide automatic archive or backup-copy cleanup.

7. Open **Members** in the guild dashboard to see the automatic initial import
   and its progress. The bot fetches the roster itself in pages of 100 at least
   15 seconds apart. About 11,000 members take roughly 28 minutes for the
   member phase before retries, other guilds and later phases. See
   [MEMBERS.md](MEMBERS.md) for completion, pause, and explicit reconciliation.

## Local validation

Run `bash scripts/ci-preflight.sh` for the full local validation suite. It builds the
bot and API executables and runs the dual-process smoke test with `--include-ignored`;
a bare `cargo test` skips that executable test. The suite also runs
`scripts/public-install-acceptance-test.sh` to check public-source documentation.
These checks do not provision a provider account or prove production cutover.

## Still-deferred live provider checks

Local tests do **not** replace:

- Live Bunny Database TLS and production credentials
- Live Magic Container deploy, private-zone confirmation, and CDN origin-pull
- Live Discord Gateway, OAuth redirect, and guild permission checks
- Live Bunny evidence zone privacy and live Discord CDN attachment fetch
- Live AWS account, VPC, and billed resource provisioning
- Provider-billed cost at current Bunny/AWS list prices
- Multi-replica session stores and writer leases

Production readiness stays blocked on those checks.
