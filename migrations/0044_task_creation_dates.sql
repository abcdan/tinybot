ALTER TABLE tasks ADD COLUMN created_at INTEGER;

CREATE TRIGGER task_creation_date AFTER INSERT ON tasks
WHEN NEW.created_at IS NULL
BEGIN
    UPDATE tasks SET created_at=NEW.updated WHERE id=NEW.id;
END;
