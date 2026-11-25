ALTER TABLE schedules ADD COLUMN catch_up TEXT NOT NULL DEFAULT 'coalesce'
    CHECK (catch_up IN ('coalesce','skip'));
ALTER TABLE schedules ADD COLUMN quiet_start INTEGER CHECK (quiet_start BETWEEN 0 AND 1439);
ALTER TABLE schedules ADD COLUMN quiet_end INTEGER CHECK (quiet_end BETWEEN 0 AND 1439);
ALTER TABLE schedules ADD COLUMN snoozed_until INTEGER;
ALTER TABLE schedules ADD COLUMN held_until INTEGER;

CREATE TABLE schedule_occurrences (
    schedule TEXT NOT NULL REFERENCES schedules(id) ON DELETE CASCADE,
    revision INTEGER NOT NULL,
    due_at INTEGER NOT NULL,
    checked_at INTEGER NOT NULL,
    outcome TEXT NOT NULL CHECK (outcome IN ('deferred','queued','skipped_overlap','skipped_missed','invalid')),
    delayed_until INTEGER,
    task TEXT REFERENCES tasks(id) ON DELETE SET NULL,
    PRIMARY KEY (schedule,revision,due_at)
);

CREATE INDEX schedule_occurrences_recent ON schedule_occurrences(schedule,due_at DESC,revision DESC);
