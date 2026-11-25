CREATE TABLE docblock_task_saves (
    approval_task TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
    request TEXT NOT NULL UNIQUE,
    source TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    source_revision INTEGER NOT NULL,
    source_event INTEGER NOT NULL,
    destination_key TEXT NOT NULL,
    path TEXT NOT NULL,
    result TEXT NOT NULL DEFAULT 'ready' CHECK(result IN ('ready','executing','saved','proposed','failed')),
    url TEXT,
    created INTEGER NOT NULL
);

CREATE INDEX docblock_task_saves_source ON docblock_task_saves(source,created DESC);
CREATE INDEX docblock_task_saves_destination ON docblock_task_saves(source,destination_key);
