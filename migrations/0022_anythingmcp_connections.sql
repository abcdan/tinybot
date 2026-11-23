ALTER TABLE connectors ADD COLUMN kind TEXT NOT NULL DEFAULT 'mcp' CHECK (kind IN ('mcp','anythingmcp'));
