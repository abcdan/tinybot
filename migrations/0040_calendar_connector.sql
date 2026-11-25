CREATE TABLE calendar_connectors (
    user TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    connector TEXT NOT NULL REFERENCES connectors(id) ON DELETE CASCADE,
    approval_epoch INTEGER NOT NULL,
    version TEXT NOT NULL,
    account TEXT NOT NULL,
    calendar TEXT NOT NULL
);
