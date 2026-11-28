CREATE INDEX outbox_queued_order ON outbox(id,retry_at)
    WHERE state='queued';
