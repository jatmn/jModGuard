# Releases and image updates

Bot, API and WebUI are independent GHCR packages. Owner Administration discovers
image tags and exact digests through the existing API. Selecting and applying an
update is an owner action; see [BUNNY-UPDATES.md](BUNNY-UPDATES.md).

## Channels

| Channel | Identity | Produced by |
|---|---|---|
| Main | Full source SHA and `latest` | Changed component inputs on trusted main pushes |
| Stable | `vMAJOR.MINOR.PATCH` | Release Please version followed by a tag/manual image build |
| Nightly | `nightly-YYYYMMDD-<sha12>` | Buildkite schedule on main |

Nightly identifiers never enter Release Please's stable version/changelog baseline.
Nightly publication must not move `latest` or a stable tag. Manual commit-SHA builds
publish all eligible components; sources predating the WebUI package skip that image.

## Publication

Buildkite is the CI publisher. Release Please runs through
`.buildkite/release-please.sh` on trusted main webhook pushes. Creating a GitHub
Release alone does not publish images; enable tag builds or select the release tag
in a manual Buildkite build.

All components in a publication use one resolved source SHA. Each image is verified
before its digest is promoted to the channel tag. Images and channels remain independent;
operators should verify component compatibility and release notes before an update.

See [Buildkite publishing](BUILDKITE.md) for source selection, registry credentials,
retry behavior and offline publication tests. Local contract tests do not push images
or prove live registry or provider rollout durability.
