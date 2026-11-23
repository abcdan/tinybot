ALTER TABLE tasks ADD COLUMN chief_chat INTEGER NOT NULL DEFAULT 1;

-- Keep the previous direct conversations and their access scope for audit.
UPDATE tasks SET chief_chat=0 WHERE agent IS NOT NULL;
UPDATE users SET active_task=NULL WHERE active_task IN (SELECT id FROM tasks WHERE chief_chat=0);
UPDATE pending SET state='superseded'
WHERE state='waiting' AND task IN (SELECT id FROM tasks WHERE chief_chat=0);
