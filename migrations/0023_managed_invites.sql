ALTER TABLE users ADD COLUMN suspended_at INTEGER;

CREATE TABLE bot_invitations (
    id TEXT PRIMARY KEY,
    token_hash TEXT NOT NULL UNIQUE,
    issuer TEXT NOT NULL REFERENCES users(id),
    expires INTEGER NOT NULL,
    member TEXT UNIQUE REFERENCES users(id),
    accepted_at INTEGER,
    revoked_at INTEGER
);
CREATE INDEX bot_invitations_issuer ON bot_invitations(issuer, expires);

CREATE TABLE bot_invitation_reviews (
    invitation TEXT NOT NULL REFERENCES bot_invitations(id) ON DELETE CASCADE,
    recipient TEXT NOT NULL,
    PRIMARY KEY(invitation, recipient)
);

INSERT INTO bot_invitations(id, token_hash, issuer, expires)
SELECT lower(hex(randomblob(16))), token, issuer, expires FROM invitations WHERE agent IS NULL;
DELETE FROM invitations WHERE agent IS NULL;

CREATE TABLE shared_memory_pairs (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL REFERENCES users(id),
    guest TEXT NOT NULL REFERENCES users(id),
    owner_opted_in INTEGER NOT NULL DEFAULT 0 CHECK(owner_opted_in IN (0, 1)),
    guest_opted_in INTEGER NOT NULL DEFAULT 0 CHECK(guest_opted_in IN (0, 1)),
    CHECK(owner <> guest),
    UNIQUE(owner, guest)
);
CREATE INDEX shared_memory_pairs_guest ON shared_memory_pairs(guest);

CREATE TABLE shared_memory_notes (
    id TEXT PRIMARY KEY,
    pair TEXT NOT NULL REFERENCES shared_memory_pairs(id) ON DELETE CASCADE,
    author TEXT NOT NULL REFERENCES users(id),
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX shared_memory_by_pair ON shared_memory_notes(pair, created_at, id);

ALTER TABLE tasks ADD COLUMN shared_memory_pair TEXT REFERENCES shared_memory_pairs(id);
CREATE INDEX shared_memory_tasks ON tasks(shared_memory_pair) WHERE shared_memory_pair IS NOT NULL;
CREATE INDEX invite_usage_tasks ON tasks(user);

CREATE VIEW available_shared_memory_pairs AS
SELECT p.id, p.owner, p.guest FROM shared_memory_pairs p
JOIN users owner ON owner.id = p.owner AND owner.suspended_at IS NULL
JOIN users guest ON guest.id = p.guest AND guest.suspended_at IS NULL
WHERE p.owner_opted_in = 1 AND p.guest_opted_in = 1;

CREATE TRIGGER revoke_shared_memory_tasks_on_leave AFTER UPDATE ON shared_memory_pairs
WHEN OLD.owner_opted_in = 1 AND OLD.guest_opted_in = 1
 AND (NEW.owner_opted_in = 0 OR NEW.guest_opted_in = 0) BEGIN
    UPDATE tasks SET state='revoked', revision=revision+1
    WHERE shared_memory_pair=OLD.id AND state<>'revoked';
END;

CREATE TRIGGER revoke_shared_memory_tasks_on_note_removal AFTER DELETE ON shared_memory_notes BEGIN
    UPDATE tasks SET state='revoked', revision=revision+1
    WHERE shared_memory_pair=OLD.pair AND state<>'revoked';
END;
