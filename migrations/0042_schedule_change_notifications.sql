ALTER TABLE schedules ADD COLUMN notify_changes INTEGER NOT NULL DEFAULT 0 CHECK (notify_changes IN (0,1));
ALTER TABLE schedule_runs ADD COLUMN notify_changes INTEGER NOT NULL DEFAULT 0 CHECK (notify_changes IN (0,1));
ALTER TABLE schedule_runs ADD COLUMN result_event INTEGER REFERENCES events(id) ON DELETE SET NULL;
ALTER TABLE schedule_runs ADD COLUMN notification_state TEXT CHECK (notification_state='unchanged');
