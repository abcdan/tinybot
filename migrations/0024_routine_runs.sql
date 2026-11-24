ALTER TABLE schedules ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;
ALTER TABLE schedules ADD COLUMN steps TEXT NOT NULL DEFAULT '[]';
ALTER TABLE schedules ADD COLUMN source TEXT;

CREATE TABLE schedule_runs (
    task TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
    schedule TEXT NOT NULL REFERENCES schedules(id) ON DELETE CASCADE,
    schedule_revision INTEGER NOT NULL,
    started_at INTEGER NOT NULL,
    trigger TEXT NOT NULL CHECK (trigger IN ('schedule','manual')),
    request_id TEXT,
    steps TEXT NOT NULL,
    source TEXT,
    results TEXT NOT NULL DEFAULT '[]',
    UNIQUE(schedule,request_id)
);
