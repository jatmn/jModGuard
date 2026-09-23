-- Update coordination lives in the shared core database. The bot owns this
-- migration; the API and service launchers open with migrate=false.
CREATE TABLE component_updates (
    component TEXT PRIMARY KEY CHECK (component IN ('bot', 'api', 'webui')),
    channel TEXT NOT NULL DEFAULT 'stable' CHECK (channel IN ('stable', 'nightly')),
    cadence TEXT NOT NULL DEFAULT 'off' CHECK (cadence IN ('off', 'daily', 'weekly', 'monthly')),
    candidate TEXT,
    checked_at INTEGER,
    check_error TEXT,
    desired TEXT,
    desired_launcher TEXT,
    installed TEXT,
    installed_launcher TEXT,
    launcher_sha256 TEXT,
    launcher_installed_at INTEGER,
    previous TEXT,
    observed TEXT,
    observed_at INTEGER,
    installed_at INTEGER,
    state TEXT NOT NULL DEFAULT 'idle',
    last_error TEXT,
    lease_owner TEXT,
    lease_until INTEGER NOT NULL DEFAULT 0,
    auto_last_digest TEXT,
    auto_last_window TEXT
);
INSERT INTO component_updates (component) VALUES ('bot'), ('api'), ('webui');

CREATE TABLE webui_managed_assets (
    source_sha TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL
);
