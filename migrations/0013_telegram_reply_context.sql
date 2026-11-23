ALTER TABLE events ADD COLUMN telegram_reply_message_id INTEGER;
ALTER TABLE events ADD COLUMN telegram_reply_excerpt TEXT;
ALTER TABLE events ADD COLUMN telegram_reply_from_bot INTEGER;

ALTER TABLE outbox ADD COLUMN reply_to_message_id INTEGER;
ALTER TABLE outbox ADD COLUMN telegram_message_id INTEGER;
ALTER TABLE outbox ADD COLUMN assistant_event_id INTEGER REFERENCES events(id) ON DELETE SET NULL;
CREATE UNIQUE INDEX outbox_telegram_message ON outbox(user, telegram_message_id)
    WHERE telegram_message_id IS NOT NULL;
