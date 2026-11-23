CREATE TABLE audit_sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    message TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    status TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0
);

ALTER TABLE tasks ADD COLUMN audit_session_id INTEGER REFERENCES audit_sessions(id) ON DELETE SET NULL;
CREATE INDEX audit_sessions_task ON audit_sessions(task,id);
CREATE INDEX audit_sessions_retention ON audit_sessions(started_at);

CREATE TABLE audit_entries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session INTEGER NOT NULL REFERENCES audit_sessions(id) ON DELETE CASCADE,
    attempt INTEGER NOT NULL,
    at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    status TEXT NOT NULL,
    detail TEXT NOT NULL
);
CREATE INDEX audit_entries_session ON audit_entries(session,id);
