CREATE TABLE deferred_results (
    invocation_id TEXT PRIMARY KEY
        REFERENCES invocations(id) ON DELETE CASCADE,
    retrieval_kind TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    cancellation_requested_at_ms INTEGER,
    terminal_override TEXT,
    failure_kind TEXT,
    failure_message TEXT
);

CREATE INDEX deferred_results_expiry
    ON deferred_results(expires_at_ms);
