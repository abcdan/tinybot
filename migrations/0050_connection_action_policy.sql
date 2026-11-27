ALTER TABLE connectors ADD COLUMN automatic_actions INTEGER NOT NULL DEFAULT 1 CHECK (automatic_actions IN (0,1));
ALTER TABLE mail_accounts ADD COLUMN automatic_actions INTEGER NOT NULL DEFAULT 1 CHECK (automatic_actions IN (0,1));
UPDATE connectors SET automatic_actions=0;
UPDATE mail_accounts SET automatic_actions=0;

CREATE TABLE receipt_rules (
    account TEXT NOT NULL,
    folder TEXT NOT NULL,
    connector TEXT NOT NULL REFERENCES connectors(id) ON DELETE CASCADE,
    connector_epoch INTEGER NOT NULL,
    workspace TEXT NOT NULL,
    failures_only INTEGER NOT NULL CHECK(failures_only IN (0,1)),
    PRIMARY KEY(account,folder),
    FOREIGN KEY(account,folder) REFERENCES mail_folders(account,name) ON DELETE CASCADE
);
