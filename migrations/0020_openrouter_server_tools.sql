CREATE TABLE openrouter_disabled_tools (
    user TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    tool TEXT NOT NULL CHECK (tool IN ('web_search','web_fetch','datetime','search_models')),
    PRIMARY KEY (user,tool)
);
