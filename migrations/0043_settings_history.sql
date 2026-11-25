CREATE TABLE settings_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    source TEXT NOT NULL CHECK(source IN ('settings','approval','restore')),
    changes TEXT NOT NULL CHECK(json_valid(changes)),
    restored_from INTEGER,
    created INTEGER NOT NULL
);
CREATE INDEX settings_history_user ON settings_history(user,id DESC);

CREATE TABLE settings_rollbacks (
    task TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
    change_id INTEGER NOT NULL
);
