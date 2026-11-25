CREATE TABLE task_artifacts (
    id TEXT PRIMARY KEY,
    task TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    user TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    version INTEGER NOT NULL,
    fingerprint TEXT NOT NULL,
    bytes INTEGER NOT NULL CHECK (bytes BETWEEN 0 AND 32768),
    ciphertext TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    UNIQUE(task, name, version)
);

CREATE INDEX task_artifact_user ON task_artifacts(user, created_at);
CREATE INDEX task_artifact_expiry ON task_artifacts(expires_at);

ALTER TABLE telegram_outputs ADD COLUMN artifact TEXT REFERENCES task_artifacts(id) ON DELETE SET NULL;
CREATE INDEX telegram_output_artifact ON telegram_outputs(artifact, outbox);
