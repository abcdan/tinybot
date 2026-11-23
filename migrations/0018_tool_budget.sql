CREATE TABLE task_tool_attempts (
    task TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    revision INTEGER NOT NULL,
    position INTEGER NOT NULL CHECK (position BETWEEN 1 AND 100),
    actor TEXT NOT NULL CHECK (actor IN ('chief','helper')),
    tool TEXT NOT NULL,
    args_fingerprint TEXT NOT NULL,
    PRIMARY KEY (task,revision,position)
);

CREATE TABLE task_tool_checks (
    task TEXT NOT NULL,
    revision INTEGER NOT NULL,
    position INTEGER NOT NULL CHECK (position BETWEEN 10 AND 100 AND position%10=0),
    stalled INTEGER NOT NULL CHECK (stalled IN (0,1)),
    PRIMARY KEY (task,revision,position),
    FOREIGN KEY (task,revision,position) REFERENCES task_tool_attempts(task,revision,position) ON DELETE CASCADE
);

CREATE TRIGGER prune_old_task_tool_budgets AFTER UPDATE OF revision ON tasks
WHEN NEW.revision<>OLD.revision BEGIN
    DELETE FROM task_tool_attempts WHERE task=NEW.id AND revision<>NEW.revision;
END;
