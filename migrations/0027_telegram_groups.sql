CREATE TABLE telegram_groups (
    chat INTEGER PRIMARY KEY CHECK(chat<0),
    requested_by TEXT NOT NULL REFERENCES users(id),
    requested_at INTEGER NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 0 CHECK(enabled IN (0,1))
);

CREATE TABLE telegram_group_tasks (
    task TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
    chat INTEGER NOT NULL REFERENCES telegram_groups(chat),
    topic INTEGER NOT NULL DEFAULT 0 CHECK(topic>=0)
);
CREATE INDEX telegram_group_scope ON telegram_group_tasks(chat,topic,task);
ALTER TABLE outbox ADD COLUMN telegram_group_message_id INTEGER;
CREATE INDEX outbox_group_receipts ON outbox(user,telegram_group_message_id)
    WHERE telegram_group_message_id IS NOT NULL;
