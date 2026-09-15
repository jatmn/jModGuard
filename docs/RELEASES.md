# Releases and update discovery

Public source produces signed Stable and Nightly publications. The conductor can
browse complete manifests by channel and exact build. Selecting a build never
deploys it. Point-and-click apply/install is a later change. There is no silent
automatic update and no Bunny registry.

## Channels

| Channel | Identity | Produced by |
|---|---|---|
| Stable | `vMAJOR.MINOR.PATCH` | Release Please on trusted `main` pushes, then a tag/manual image build |
| Nightly | `nightly-YYYYMMDD-<sha12>` | Buildkite `schedule` on `main` |

Nightly identifiers never enter Release Please's stable version/changelog
baseline. Unique nightly image tags must not move `latest` or a stable tag.
Main SHA publication still publishes bot/API images when inputs change; it is
not an installable release and does not publish a conductor image or complete
manifest.

## Publication

Buildkite remains the only CI. Release Please runs from
`.buildkite/release-please.py` using `npx` on trusted main webhook pushes. It
does not restore GitHub Actions image workflows.

Complete installable publications require **bot, API, conductor and frontend**
artifacts. Interrupted or partial sets stay unavailable; they are not listed for
discovery. Manifests use format `JMODGUARD-RELEASE-1` and an index
`JMODGUARD-RELEASE-INDEX-1`. Canonical JSON is compact with sorted object keys
so Python CI and the conductor verify the same bytes.

Signing is HMAC-SHA256 with a `key_id`. The 32-byte hex secret lives in
`JMODGUARD_RELEASE_SIGNING_KEY` / Buildkite secrets, not git.
`docs/release-trust.json` records key ids for rotation and keeps `keys` empty in
source. Retired ids must not verify. The conductor loads live secrets from
`JMODGUARD_CONDUCTOR_RELEASE_KEYS` (JSON object of `key_id` → hex secret).

Compatibility records the core/guild family and version, settings schema and
backup format (`JMODGUARD-BACKUP-1`). An older schema cannot replace a newer
installation; restore a matching backup instead.

## Conductor discovery

| Environment variable | Purpose |
|---|---|
| `JMODGUARD_CONDUCTOR_RELEASE_FEED` | HTTPS/HTTP index URL. Unset keeps discovery inactive. |
| `JMODGUARD_CONDUCTOR_RELEASE_KEYS` | JSON map of current HMAC secrets. |
| `JMODGUARD_CONDUCTOR_INSTALLED_RELEASE` | Optional installed release id. Defaults to `v` plus the crate version. |

`/version` keeps `update_available: false`. `discovery_available` is true only
when operations, a feed and at least one key are configured. `GET /api/updates`
returns the cached catalog and polls the feed when the background interval has
elapsed (or immediately when `?refresh=true`). It verifies signatures, skips
invalid/incomplete artifacts and returns the catalog. `POST /api/updates` and
`POST /api/updates/apply` return 404. Owner settings select Stable/Nightly
channels and a background poll interval.
Enabling the `updates` notification category can report a newly seen compatible
build; that is not an install.

Local tests cover complete and interrupted publication, channel isolation,
unknown/retired keys, older-schema refusal and available-update counts. They
do not publish images, deploy containers or prove live registry durability.

See [Buildkite publishing](BUILDKITE.md) and [conductor](CONDUCTOR.md).
