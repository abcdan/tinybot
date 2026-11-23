ALTER TABLE events ADD COLUMN telegram_message_id INTEGER;
ALTER TABLE events ADD COLUMN telegram_reaction_queued INTEGER NOT NULL DEFAULT 0;
ALTER TABLE outbox ADD COLUMN reaction_event INTEGER REFERENCES events(id) ON DELETE CASCADE;
