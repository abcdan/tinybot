CREATE INDEX outbox_assistant_events ON outbox(assistant_event_id, id)
WHERE assistant_event_id IS NOT NULL;
