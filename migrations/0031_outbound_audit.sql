ALTER TABLE audit_sessions ADD COLUMN received_at_ms INTEGER;
ALTER TABLE audit_sessions ADD COLUMN first_attempt_at_ms INTEGER;
ALTER TABLE audit_sessions ADD COLUMN first_draft_at_ms INTEGER;

CREATE TABLE audit_deliveries (
    outbox INTEGER PRIMARY KEY,
    session INTEGER NOT NULL REFERENCES audit_sessions(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK(kind IN ('reply','reaction','progress','decision','failure_notice')),
    state TEXT NOT NULL DEFAULT 'queued' CHECK(state IN ('queued','sent','failed','uncertain','skipped')),
    queued_at_ms INTEGER NOT NULL,
    finished_at_ms INTEGER,
    deferrals INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX audit_deliveries_session ON audit_deliveries(session);

-- New receipts are joined to the turn at enqueue time. They survive ordinary
-- outbox cleanup for the rest of the audit retention window.
CREATE TRIGGER audit_outbox_enqueued AFTER INSERT ON outbox WHEN
    NEW.assistant_event_id IS NOT NULL OR NEW.reaction_event IS NOT NULL
    OR NEW.button LIKE 'ack-generate/%' OR NEW.button LIKE 'retry-turn/%'
    OR NEW.button LIKE 'form/%' OR NEW.button LIKE 'choice/%'
    OR NEW.button LIKE 'settings-form/%' OR NEW.button LIKE 'mcp-approval/%'
    OR NEW.button LIKE 'mcp-approval-once/%' OR NEW.button LIKE 'mail-send/%'
BEGIN
    INSERT INTO audit_deliveries(outbox,session,kind,queued_at_ms)
    SELECT NEW.id, s.id,
        CASE
            WHEN NEW.assistant_event_id IS NOT NULL THEN 'reply'
            WHEN NEW.reaction_event IS NOT NULL THEN 'reaction'
            WHEN NEW.button LIKE 'ack-generate/%' THEN 'progress'
            WHEN NEW.button LIKE 'retry-turn/%' THEN 'failure_notice'
            ELSE 'decision'
        END,
        CAST(unixepoch('subsec') * 1000 AS INTEGER)
    FROM tasks t JOIN audit_sessions s ON s.id=t.audit_session_id
    WHERE t.id=NEW.task AND t.user=NEW.user
      AND s.started_at>unixepoch()-604800;
END;

CREATE TRIGGER audit_progress_retargeted AFTER UPDATE OF reply_to_message_id ON outbox
WHEN NEW.button LIKE 'ack-generate/%' AND NEW.reply_to_message_id IS NOT OLD.reply_to_message_id
BEGIN
    INSERT INTO audit_deliveries(outbox,session,kind,queued_at_ms)
    SELECT NEW.id, s.id, 'progress', CAST(unixepoch('subsec') * 1000 AS INTEGER)
    FROM tasks t JOIN audit_sessions s ON s.id=t.audit_session_id
    WHERE t.id=NEW.task AND t.user=NEW.user AND s.started_at>unixepoch()-604800
    ON CONFLICT(outbox) DO UPDATE SET session=excluded.session,queued_at_ms=excluded.queued_at_ms,
                                    state='queued',finished_at_ms=NULL,deferrals=0;
END;

CREATE TRIGGER audit_outbox_transition AFTER UPDATE OF state ON outbox
WHEN OLD.state<>NEW.state
BEGIN
    UPDATE audit_deliveries SET deferrals=deferrals+1
      WHERE outbox=NEW.id AND OLD.state='sending' AND NEW.state='queued';
    UPDATE audit_deliveries SET state=CASE
        WHEN NEW.state='failed' AND OLD.state<>'sending' THEN 'skipped' ELSE NEW.state END,
        finished_at_ms=CAST(unixepoch('subsec') * 1000 AS INTEGER)
    WHERE outbox=NEW.id AND NEW.state IN ('sent','failed','uncertain','skipped');
END;
