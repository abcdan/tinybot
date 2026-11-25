ALTER TABLE schedule_occurrences RENAME TO old_schedule_occurrences;

CREATE TABLE schedule_occurrences (
    schedule TEXT NOT NULL REFERENCES schedules(id) ON DELETE CASCADE,
    revision INTEGER NOT NULL,
    due_at INTEGER NOT NULL,
    checked_at INTEGER NOT NULL,
    outcome TEXT NOT NULL CHECK (outcome IN ('deferred','queued','skipped_overlap','skipped_missed','coalesced','invalid')),
    delayed_until INTEGER,
    task TEXT REFERENCES tasks(id) ON DELETE SET NULL,
    PRIMARY KEY (schedule,revision,due_at)
);

INSERT INTO schedule_occurrences(schedule,revision,due_at,checked_at,outcome,delayed_until,task)
SELECT schedule,revision,due_at,checked_at,outcome,delayed_until,task FROM old_schedule_occurrences;

DROP TABLE old_schedule_occurrences;
CREATE INDEX schedule_occurrences_recent ON schedule_occurrences(schedule,due_at DESC,revision DESC);
