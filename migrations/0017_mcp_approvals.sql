ALTER TABLE connectors ADD COLUMN approval_epoch INTEGER NOT NULL DEFAULT 0;

CREATE TABLE mcp_approvals (
    owner TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    connector TEXT NOT NULL REFERENCES connectors(id) ON DELETE CASCADE,
    tool TEXT NOT NULL,
    epoch INTEGER NOT NULL,
    PRIMARY KEY (owner, connector, tool)
);

CREATE INDEX mcp_approvals_connector ON mcp_approvals(connector);

CREATE TRIGGER invalidate_mcp_approvals AFTER UPDATE OF approval_epoch,owner,endpoint,transport,auth ON connectors
WHEN NEW.approval_epoch<>OLD.approval_epoch OR NEW.owner<>OLD.owner OR NEW.endpoint<>OLD.endpoint
    OR NEW.transport<>OLD.transport OR NEW.auth<>OLD.auth BEGIN
    UPDATE connectors SET approval_epoch=OLD.approval_epoch+1
    WHERE id=NEW.id AND approval_epoch=OLD.approval_epoch;
    DELETE FROM mcp_approvals WHERE connector=NEW.id;
END;
