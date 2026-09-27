# libsql 0.9.30 HTTP/1.1 feature patch

Source: https://crates.io/crates/libsql/0.9.30
Published archive SHA-256: `30fe980ac5693ed1f3db490559fb578885e913a018df64af8a1a46e1959a78df`.
Upstream commit: `0653c5788d77ef16a97c56ff3e9fdc11717a72d9`, package directory `libsql`.
License: MIT; `LICENSE.md` copied from that upstream commit.

The published package is retained verbatim except:

- `Cargo.toml`: remove `http2` from `[dependencies.hyper].features`.
- Omit the upstream development `Cargo.lock`; the repository root lockfile controls this build.
- Normalize upstream trailing whitespace in `src/replication/client.rs`,
  `src/replication/connection.rs`, `src/sync/statement.rs` and
  `tests/integration_tests.rs` for the repository whitespace gate.
- Add this provenance note and the upstream license text.

`Cargo.toml.orig` records the original upstream workspace manifest, not the active
package manifest. Rust logic and upstream tests are retained; the only source
normalization is the whitespace listed above.

libSQL enables legacy Hyper HTTP/2 even for remote Hrana requests. That pulls
unpatched `h2` 0.3.27 (RUSTSEC-2026-0258 / GHSA-q83h-524g-xf6h).
jModGuard's remote database, evidence and image clients already use HTTP/1.1;
the in-tree TLS connector advertises only `http/1.1`. Selected libSQL features
remain `core` and `remote`, with default `replication`, `sync` and `tls` off.
This patch removes unused protocol code without changing libSQL Rust code,
API, database engine or wire requests.

The shared-core dependency boundary rejects `h2`. Other HTTP clients retain
HTTP/2 through patched `h2` 0.4 in the root lockfile. Additional libSQL features
may reintroduce the old stack and require a new security/compatibility review.
Do not suppress the advisory.

When upgrading libSQL, compare Hyper features and re-evaluate this patch.
Remove it when a supported upstream release no longer pulls vulnerable `h2`.
Run full preflight and the remote-database job's local libSQL container tests.
