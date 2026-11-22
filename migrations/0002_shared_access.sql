CREATE TRIGGER revoke_agent_member_tasks AFTER DELETE ON grants BEGIN
    UPDATE tasks SET state='revoked',revision=revision+1 WHERE agent=OLD.agent AND user=OLD.member;
END;

CREATE TRIGGER revoke_connector_read_tasks AFTER DELETE ON connector_grants BEGIN
    UPDATE tasks SET state='revoked',revision=revision+1 WHERE agent=OLD.agent
        AND user<>(SELECT owner FROM agents WHERE id=OLD.agent);
END;

CREATE TRIGGER change_connector_read_tasks AFTER UPDATE OF scope ON connector_grants
WHEN NEW.scope<>OLD.scope BEGIN
    UPDATE tasks SET state='revoked',revision=revision+1 WHERE agent=OLD.agent
        AND user<>(SELECT owner FROM agents WHERE id=OLD.agent);
END;

CREATE TRIGGER revoke_published_memory_tasks AFTER DELETE ON memory WHEN OLD.agent IS NOT NULL BEGIN
    UPDATE tasks SET state='revoked',revision=revision+1 WHERE agent=OLD.agent AND user<>OLD.owner;
END;
