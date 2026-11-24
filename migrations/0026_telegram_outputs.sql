CREATE TABLE telegram_outputs (
    outbox INTEGER PRIMARY KEY REFERENCES outbox(id) ON DELETE CASCADE,
    revision INTEGER NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('document','voice')),
    name TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    ciphertext TEXT,
    audio TEXT
);

CREATE TRIGGER finish_telegram_output AFTER UPDATE OF state ON outbox
WHEN NEW.state<>OLD.state AND NEW.state IN ('sent','failed','uncertain')
  AND EXISTS (SELECT 1 FROM telegram_outputs WHERE outbox=NEW.id)
BEGIN
    UPDATE telegram_outputs SET ciphertext=NULL,audio=NULL WHERE outbox=NEW.id;
    INSERT INTO outbox(user,task,agent,body)
        SELECT NEW.user,NEW.task,NEW.agent,
            CASE NEW.state WHEN 'uncertain'
                THEN 'Telegram may have received the file. Check this chat before requesting it again.'
                ELSE 'TinyBot could not deliver the file. You can request it again.' END
        WHERE NEW.state IN ('failed','uncertain')
          AND EXISTS(SELECT 1 FROM tasks t JOIN telegram_outputs f ON f.outbox=NEW.id
                     WHERE t.id=NEW.task AND t.state NOT IN ('cancelled','revoked')
                         AND t.revision=f.revision);
END;
