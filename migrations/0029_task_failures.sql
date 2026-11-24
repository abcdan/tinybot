CREATE TABLE task_failures (
    task TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
    token TEXT NOT NULL UNIQUE,
    revision INTEGER NOT NULL,
    message TEXT NOT NULL,
    retryable INTEGER NOT NULL CHECK(retryable IN (0,1))
);

CREATE TRIGGER clear_task_failure_when_replaced AFTER UPDATE OF revision,state ON tasks
WHEN NEW.revision<>OLD.revision OR (OLD.state='blocked' AND NEW.state<>'blocked')
BEGIN
    DELETE FROM task_failures WHERE task=NEW.id;
END;
