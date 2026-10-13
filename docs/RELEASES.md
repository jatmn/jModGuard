# Releases and image updates

Bot, API and WebUI are independent GHCR packages. Owner Administration discovers
image tags and exact digests through the existing API. Selecting and applying an
update is an owner action; see [BUNNY-UPDATES.md](BUNNY-UPDATES.md).

## Channels

| Channel | Identity | Produced by |
|---|---|---|
| Main | Full source SHA and `latest` | Changed component inputs on trusted main pushes |
| Stable | `vMAJOR.MINOR.PATCH` | Release Please version followed by a tag/manual image build |
| Nightly | `nightly-YYYYMMDD-<sha12>` | GitHub Actions schedule on main |

Nightly identifiers never enter Release Please's stable version/changelog baseline.
Nightly publication must not move `latest` or a stable tag. Manual commit-SHA builds
publish all eligible components; sources predating the WebUI package skip that image.

## Publication

GitHub Actions runs the pinned Release Please action after trusted main checks.
It opens/updates release PRs; the owner merges them. The resulting release tag
is published in the same workflow using the release's reported source SHA,
which need not equal the workflow's triggering commit. If it differs, all existing
CI check groups must pass at that pinned source before any component publishes.
The retained plan preserves that tag and source on retries, with the same validation
rule. Release Please runs only on the first attempt; retries never create another
release. Owner-pushed `v*` tags also publish after checks. Creating a GitHub Release manually does not start a separate build;
select its tag as `source` in an owner-dispatched **Publish** run on `main`.

If a release was created before the action, preparation, or plan-artifact upload
failed, a retry without that plan is refused. Recover with a fresh owner-dispatched
**Publish** run on `main` using `source=refs/tags/<created-tag>`. An empty source only
runs checks, and an ordinary main run cannot recreate that release's output.

All components in a publication use one resolved source SHA. Each image is verified
before its digest is promoted to the channel tag. Images and channels remain independent;
operators should verify component compatibility and release notes before an update.

See [CI and publishing](CI.md) for source selection, registry credentials,
retry behavior and offline publication tests. Local contract tests do not push images
or prove live registry or provider rollout durability.
