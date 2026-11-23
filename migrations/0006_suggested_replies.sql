CREATE TABLE suggested_replies (
    token TEXT PRIMARY KEY,
    outbox INTEGER NOT NULL REFERENCES outbox(id) ON DELETE CASCADE,
    revision INTEGER NOT NULL,
    body TEXT NOT NULL,
    expires INTEGER NOT NULL
);
CREATE INDEX suggested_replies_outbox ON suggested_replies(outbox);
CREATE INDEX suggested_replies_expiry ON suggested_replies(expires);
