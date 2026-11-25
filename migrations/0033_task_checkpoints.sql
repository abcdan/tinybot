CREATE TABLE task_plan_steps (
    task TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    revision INTEGER NOT NULL,
    position INTEGER NOT NULL CHECK (position BETWEEN 0 AND 7),
    title TEXT NOT NULL,
    checkpoint TEXT,
    completed_at INTEGER,
    PRIMARY KEY (task, revision, position)
);

CREATE TABLE task_verified_reads (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    revision INTEGER NOT NULL,
    lookup_key TEXT NOT NULL,
    recorded_at INTEGER NOT NULL,
    ciphertext TEXT NOT NULL,
    UNIQUE (task, revision, lookup_key)
);

CREATE TABLE task_model_passes (
    task TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    revision INTEGER NOT NULL,
    passes INTEGER NOT NULL,
    PRIMARY KEY (task, revision)
);

CREATE TRIGGER prune_task_checkpoints AFTER UPDATE OF revision ON tasks
WHEN NEW.revision<>OLD.revision BEGIN
    DELETE FROM task_plan_steps WHERE task=NEW.id AND revision<>NEW.revision
        AND NEW.state<>'completed';
    DELETE FROM task_verified_reads WHERE task=NEW.id AND revision<>NEW.revision;
    DELETE FROM task_model_passes WHERE task=NEW.id AND revision<>NEW.revision;
END;
