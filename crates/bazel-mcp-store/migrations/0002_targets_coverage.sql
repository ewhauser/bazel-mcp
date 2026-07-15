CREATE TABLE IF NOT EXISTS target_results (
    invocation_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    label TEXT NOT NULL,
    success INTEGER NOT NULL,
    record_json TEXT NOT NULL,
    PRIMARY KEY(invocation_id, ordinal)
);

CREATE TABLE IF NOT EXISTS coverage_files (
    invocation_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    path TEXT NOT NULL,
    record_json TEXT NOT NULL,
    PRIMARY KEY(invocation_id, ordinal)
);

CREATE INDEX IF NOT EXISTS target_results_invocation
    ON target_results(invocation_id, ordinal);
CREATE INDEX IF NOT EXISTS coverage_files_invocation
    ON coverage_files(invocation_id, ordinal);
