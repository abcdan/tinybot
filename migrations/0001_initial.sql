CREATE TABLE users (
    id TEXT PRIMARY KEY, active_task TEXT, provider TEXT, model TEXT,
    credential TEXT, host_id TEXT NOT NULL, onboarded_at INTEGER NOT NULL
);
CREATE TABLE agents (
    id TEXT PRIMARY KEY, owner TEXT NOT NULL REFERENCES users(id),
    name TEXT NOT NULL, instructions TEXT NOT NULL, tools TEXT NOT NULL,
    UNIQUE(owner, name)
);
CREATE TABLE grants (
    agent TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    member TEXT NOT NULL REFERENCES users(id), PRIMARY KEY (agent, member)
);
CREATE TABLE invitations (
    token TEXT PRIMARY KEY, issuer TEXT NOT NULL REFERENCES users(id),
    agent TEXT REFERENCES agents(id) ON DELETE CASCADE, expires INTEGER NOT NULL
);
CREATE TABLE tasks (
    id TEXT PRIMARY KEY, user TEXT NOT NULL REFERENCES users(id),
    agent TEXT REFERENCES agents(id), title TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'queued', revision INTEGER NOT NULL DEFAULT 0,
    summary TEXT NOT NULL DEFAULT '', goal TEXT NOT NULL DEFAULT '',
    updated INTEGER NOT NULL
);
CREATE INDEX task_queue ON tasks(state, updated);
CREATE TABLE events (
    id INTEGER PRIMARY KEY AUTOINCREMENT, task TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    role TEXT NOT NULL, body TEXT NOT NULL
);
CREATE INDEX events_task ON events(task,id);
CREATE TABLE memory (
    id TEXT PRIMARY KEY, owner TEXT NOT NULL REFERENCES users(id),
    agent TEXT REFERENCES agents(id) ON DELETE CASCADE, body TEXT NOT NULL
);
CREATE INDEX memory_scope ON memory(owner,agent);
CREATE TABLE pending (
    id TEXT PRIMARY KEY, user TEXT NOT NULL REFERENCES users(id),
    task TEXT REFERENCES tasks(id) ON DELETE CASCADE, revision INTEGER,
    kind TEXT NOT NULL, payload TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'waiting',
    expires INTEGER NOT NULL
);
CREATE INDEX pending_task ON pending(task,state);
CREATE TABLE connectors (
    id TEXT PRIMARY KEY, owner TEXT NOT NULL REFERENCES users(id),
    name TEXT NOT NULL, endpoint TEXT NOT NULL, auth TEXT NOT NULL,
    credential TEXT, UNIQUE(owner,name)
);
CREATE TABLE connector_grants (
    agent TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    connector TEXT NOT NULL REFERENCES connectors(id) ON DELETE CASCADE,
    tool TEXT NOT NULL, scope TEXT NOT NULL, PRIMARY KEY (agent, connector, tool)
);
CREATE TABLE updates (channel TEXT NOT NULL, external_id INTEGER NOT NULL,
    PRIMARY KEY(channel,external_id));
CREATE TABLE outbox (
    id INTEGER PRIMARY KEY AUTOINCREMENT, user TEXT NOT NULL REFERENCES users(id),
    task TEXT REFERENCES tasks(id) ON DELETE CASCADE, agent TEXT REFERENCES agents(id),
    body TEXT NOT NULL, button TEXT, state TEXT NOT NULL DEFAULT 'queued',
    retry_at INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE files (task TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    name TEXT NOT NULL, body TEXT NOT NULL, PRIMARY KEY (task,name));
