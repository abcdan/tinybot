CREATE TABLE telegram_attachments (
    event INTEGER PRIMARY KEY REFERENCES events(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    mime TEXT NOT NULL,
    ciphertext TEXT NOT NULL
);
