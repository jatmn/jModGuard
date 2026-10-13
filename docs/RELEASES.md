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
which need not equal the workflow's triggering commit. The retained plan preserves
that tag and source on retries. Owner-pushed `v*` tags also publish after
checks. Creating a GitHub Release manually does not start a separate build;
select its tag as `source` in an owner-dispatched CI run on `main`.

All components in a publication use one resolved source SHA. Each image is verified
before its digest is promoted to the channel tag. Images and channels remain independent;
operators should verify component compatibility and release notes before an update.

See [CI and publishing](CI.md) for source selection, registry credentials,
retry behavior and offline publication tests. Local contract tests do not push images
or prove live registry or provider rollout durability.
