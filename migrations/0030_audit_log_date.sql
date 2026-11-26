-- Index the same UTC millisecond key used for date filtering and pagination.
-- Clock-relative keywords have no saved time and cannot be evaluated in an
-- index. SQLite's date parser stops at a NUL, as does substr on text.
ALTER TABLE events ADD COLUMN list_date INTEGER AS (
    CASE
        WHEN substr(CAST(created_at AS TEXT), 1, 10) COLLATE NOCASE
             IN ('now', 'subsec', 'subsecond') THEN 0
        ELSE COALESCE(CAST(ROUND((julianday(created_at) - 2440587.5)
                              * 86400000) AS INTEGER), 0)
    END
) VIRTUAL;

CREATE INDEX idx_events_list_date ON events(guild_id, list_date, id);
