ALTER TABLE users ADD COLUMN time_zone TEXT;
ALTER TABLE tasks ADD COLUMN schedule TEXT;

CREATE TABLE schedules (
    id TEXT PRIMARY KEY, owner TEXT NOT NULL REFERENCES users(id),
    title TEXT NOT NULL, input TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('reminder','task')),
    cron TEXT, time_zone TEXT NOT NULL,
    next_run INTEGER, state TEXT NOT NULL DEFAULT 'active'
);
CREATE INDEX schedules_due ON schedules(state,next_run);
CREATE INDEX schedules_owner ON schedules(owner,state);
CREATE INDEX tasks_schedule ON tasks(schedule,state);
