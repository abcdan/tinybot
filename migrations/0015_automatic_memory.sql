ALTER TABLE agent_memory ADD COLUMN automatic INTEGER NOT NULL DEFAULT 0 CHECK (automatic IN (0, 1));
ALTER TABLE agent_memory ADD COLUMN archived_at INTEGER;

DROP INDEX agent_memory_entry;
CREATE UNIQUE INDEX agent_memory_entry ON agent_memory(owner, scope, coalesce(agent, ''), body)
    WHERE archived_at IS NULL;
