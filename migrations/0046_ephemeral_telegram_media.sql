ALTER TABLE telegram_outputs ADD COLUMN media_format TEXT
    CHECK (media_format IS NULL OR (kind='document' AND media_format IN ('png','jpeg','webp','mp4')));
ALTER TABLE telegram_outputs ADD COLUMN media_expires_at INTEGER;

CREATE INDEX telegram_output_media_expiry ON telegram_outputs(media_expires_at)
    WHERE media_expires_at IS NOT NULL;

CREATE TRIGGER audit_telegram_media_enqueued AFTER INSERT ON telegram_outputs
WHEN NEW.media_format IS NOT NULL
BEGIN
    INSERT INTO audit_deliveries(outbox,session,kind,queued_at_ms)
    SELECT o.id, s.id, 'reply', CAST(unixepoch('subsec') * 1000 AS INTEGER)
    FROM outbox o JOIN tasks t ON t.id=o.task AND t.user=o.user
    JOIN audit_sessions s ON s.id=t.audit_session_id
    WHERE o.id=NEW.outbox AND s.started_at>unixepoch()-604800;
END;

CREATE TRIGGER discard_skipped_telegram_media AFTER UPDATE OF state ON outbox
WHEN NEW.state<>OLD.state AND NEW.state='skipped'
BEGIN
    UPDATE telegram_outputs SET ciphertext=NULL,audio=NULL
      WHERE outbox=NEW.id AND media_format IS NOT NULL;
END;
