CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS invocations (
    id TEXT PRIMARY KEY NOT NULL,
    workspace TEXT NOT NULL,
    command TEXT NOT NULL,
    state TEXT NOT NULL,
    requested_at_ms INTEGER NOT NULL,
    started_at_ms INTEGER,
    finished_at_ms INTEGER,
    request_json TEXT NOT NULL,
    termination_json TEXT,
    summary_json TEXT,
    metrics_json TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS invocations_workspace_time
    ON invocations(workspace, requested_at_ms DESC, id DESC);
CREATE INDEX IF NOT EXISTS invocations_state
    ON invocations(state, requested_at_ms DESC);

CREATE TABLE IF NOT EXISTS diagnostics (
    invocation_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    severity TEXT NOT NULL,
    category TEXT NOT NULL,
    message TEXT NOT NULL,
    target TEXT,
    record_json TEXT NOT NULL,
    PRIMARY KEY(invocation_id, ordinal)
);

CREATE INDEX IF NOT EXISTS diagnostics_invocation
    ON diagnostics(invocation_id, ordinal);

CREATE TABLE IF NOT EXISTS test_results (
    invocation_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    label TEXT NOT NULL,
    status TEXT NOT NULL,
    record_json TEXT NOT NULL,
    PRIMARY KEY(invocation_id, ordinal)
);

CREATE TABLE IF NOT EXISTS query_rows (
    invocation_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY(invocation_id, ordinal)
);

CREATE TABLE IF NOT EXISTS artifacts (
    invocation_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    name TEXT NOT NULL,
    uri TEXT NOT NULL,
    record_json TEXT NOT NULL,
    PRIMARY KEY(invocation_id, ordinal)
);

