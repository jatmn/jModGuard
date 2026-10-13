# Vendored libsql 0.9.30

Upstream: [tursodatabase/libsql](https://github.com/tursodatabase/libsql) release `0.9.30`.

This copy matches the published crate except the optional `hyper` dependency no longer
enables the `http2` feature. jModGuard remote HTTPS uses HTTP/1.1 only (see
`src/remote_https.rs`); libSQL does not negotiate HTTP/2 on that path. Dropping the
feature removes the unpatched `h2` 0.3 line from the lockfile while keeping libSQL
0.9.30 behavior.

Remove this vendor when a compatible upstream libSQL release no longer pulls `h2` 0.3,
or when the 0.3 line has a published security fix we can depend on directly.
