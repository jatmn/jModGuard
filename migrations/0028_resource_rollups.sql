-- Owner Bot Resources: per-process resource rollups and the bot's latest
-- status. Only the bot writes these rows; the API reads them for the bot
-- owner. Minute rows are kept 48 hours and hour rows 31 days; every write
-- prunes older rows. Values are this application's own measurements, never
-- provider billing data.
CREATE TABLE resource_rollups (
    process TEXT NOT NULL CHECK(process IN ('bot', 'api')),
    span_secs INTEGER NOT NULL CHECK(span_secs IN (60, 3600)),
    start_secs INTEGER NOT NULL CHECK(typeof(start_secs) = 'integer' AND start_secs > 0 AND start_secs % span_secs = 0),
    data TEXT NOT NULL CHECK(json_valid(data) AND length(data) <= 16384),
    PRIMARY KEY (process, span_secs, start_secs)
) WITHOUT ROWID;

CREATE TABLE resource_status (
    process TEXT PRIMARY KEY CHECK(process IN ('bot', 'api')),
    updated_at_ms INTEGER NOT NULL CHECK(typeof(updated_at_ms) = 'integer' AND updated_at_ms > 0),
    data TEXT NOT NULL CHECK(json_valid(data) AND length(data) <= 16384)
);
