CREATE TABLE routine_webhooks (
    id TEXT PRIMARY KEY,
    schedule TEXT NOT NULL UNIQUE REFERENCES schedules(id) ON DELETE CASCADE,
    owner TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    schedule_revision INTEGER NOT NULL,
    signing_key TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
