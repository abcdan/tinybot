ALTER TABLE users ADD COLUMN backup_provider TEXT CHECK (backup_provider IN ('openrouter','codex'));
ALTER TABLE users ADD COLUMN backup_model TEXT;

CREATE TABLE task_model_backups (
    task TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    revision INTEGER NOT NULL,
    provider TEXT NOT NULL CHECK (provider IN ('openrouter','codex')),
    model TEXT NOT NULL,
    PRIMARY KEY (task, revision)
);

CREATE TRIGGER prune_task_model_backups AFTER UPDATE OF revision ON tasks
WHEN NEW.revision<>OLD.revision BEGIN
    DELETE FROM task_model_backups WHERE task=NEW.id AND revision<>NEW.revision;
END;
