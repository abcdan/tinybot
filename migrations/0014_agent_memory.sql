CREATE TABLE agent_memory (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    owner TEXT NOT NULL REFERENCES users(id),
    scope TEXT NOT NULL CHECK (scope IN ('chief', 'helper', 'group')),
    agent TEXT REFERENCES agents(id) ON DELETE CASCADE,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    CHECK ((scope = 'helper') = (agent IS NOT NULL))
);
CREATE UNIQUE INDEX agent_memory_entry ON agent_memory(owner, scope, coalesce(agent, ''), body);
CREATE INDEX agent_memory_page ON agent_memory(owner, scope, agent, sequence DESC);

-- Chief notes have no old timestamp. Published helper notes retain their table,
-- IDs, and revocation trigger.
INSERT OR IGNORE INTO agent_memory(id, owner, scope, body, created_at)
SELECT id, owner, 'chief', trim(body, char(9, 10, 11, 12, 13, 32)), 0
FROM memory WHERE agent IS NULL ORDER BY rowid;
DELETE FROM memory WHERE agent IS NULL;
