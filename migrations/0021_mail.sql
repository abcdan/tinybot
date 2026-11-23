CREATE TABLE mail_accounts (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    label TEXT NOT NULL,
    address TEXT NOT NULL,
    epoch INTEGER NOT NULL DEFAULT 0,
    can_send INTEGER NOT NULL,
    encrypted TEXT NOT NULL
);

CREATE INDEX mail_accounts_owner ON mail_accounts(owner);

CREATE TABLE mail_folders (
    account TEXT NOT NULL REFERENCES mail_accounts(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    automate INTEGER NOT NULL DEFAULT 0,
    uid_validity INTEGER,
    last_uid INTEGER NOT NULL DEFAULT 0,
    next_poll INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(account,name)
);

CREATE INDEX mail_folders_poll ON mail_folders(next_poll);

CREATE TABLE mail_messages (
    id TEXT PRIMARY KEY,
    account TEXT NOT NULL REFERENCES mail_accounts(id) ON DELETE CASCADE,
    account_epoch INTEGER NOT NULL,
    folder TEXT NOT NULL,
    uid_validity INTEGER NOT NULL,
    uid INTEGER NOT NULL,
    task TEXT NOT NULL UNIQUE REFERENCES tasks(id) ON DELETE CASCADE,
    notice INTEGER REFERENCES outbox(id) ON DELETE SET NULL,
    preview INTEGER REFERENCES outbox(id) ON DELETE SET NULL,
    revealed INTEGER NOT NULL DEFAULT 0,
    automated_revision INTEGER,
    encrypted TEXT NOT NULL,
    UNIQUE(account,account_epoch,folder,uid_validity,uid)
);

CREATE INDEX mail_messages_notice ON mail_messages(notice);
CREATE INDEX mail_messages_preview ON mail_messages(preview);
