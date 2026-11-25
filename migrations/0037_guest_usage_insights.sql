ALTER TABLE bot_invitations ADD COLUMN insights_version INTEGER NOT NULL DEFAULT 1
    CHECK(insights_version IN (1,2));
ALTER TABLE users ADD COLUMN first_result_at_ms INTEGER;

CREATE VIEW account_delivered_runs AS
SELECT s.id AS session,t.user,MAX(d.finished_at_ms) AS delivered_at_ms
FROM audit_sessions s JOIN tasks t ON t.id=s.task
JOIN audit_deliveries d ON d.session=s.id AND d.kind IN ('reply','reaction')
WHERE s.status='completed'
GROUP BY s.id
HAVING COUNT(*)=SUM(d.state='sent') AND delivered_at_ms IS NOT NULL;

UPDATE users SET first_result_at_ms=(
    SELECT MIN(run.delivered_at_ms) FROM account_delivered_runs run WHERE run.user=users.id
);

-- A reaction can be delivered before the run finishes. A reply is often the
-- opposite. Check both transitions and latch the first confirmed result.
CREATE TRIGGER user_first_result_from_delivery AFTER UPDATE OF state ON audit_deliveries
WHEN NEW.state='sent' AND OLD.state<>'sent' AND NEW.kind IN ('reply','reaction')
BEGIN
    UPDATE users SET first_result_at_ms=(
        SELECT delivered_at_ms FROM account_delivered_runs WHERE session=NEW.session
    ) WHERE first_result_at_ms IS NULL
       AND id=(SELECT user FROM account_delivered_runs WHERE session=NEW.session);
END;

CREATE TRIGGER user_first_result_from_run AFTER UPDATE OF status ON audit_sessions
WHEN NEW.status='completed' AND OLD.status<>'completed'
BEGIN
    UPDATE users SET first_result_at_ms=(
        SELECT delivered_at_ms FROM account_delivered_runs WHERE session=NEW.id
    ) WHERE first_result_at_ms IS NULL
       AND id=(SELECT user FROM account_delivered_runs WHERE session=NEW.id);
END;

CREATE TABLE usage_alerts (
    user TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    weekly_cents INTEGER CHECK(weekly_cents BETWEEN 1 AND 1000000),
    notified_week INTEGER
);
