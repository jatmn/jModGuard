-- UTC monthly snapshots of scoped service statistics. Resource IDs and API
-- keys never enter this table. The digest keeps a changed source's data apart.
CREATE TABLE bunny_cost_snapshots (
    service TEXT NOT NULL,
    month INTEGER NOT NULL,
    source_digest BLOB NOT NULL CHECK(length(source_digest) = 32),
    fetched_at INTEGER NOT NULL,
    data TEXT NOT NULL CHECK(length(data) <= 16384),
    PRIMARY KEY (service, source_digest, month)
) WITHOUT ROWID;
