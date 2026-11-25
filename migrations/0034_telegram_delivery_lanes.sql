ALTER TABLE outbox ADD COLUMN delivery_lane TEXT;

UPDATE outbox SET delivery_lane=coalesce(
    (SELECT 'group:' || g.chat FROM telegram_group_tasks g WHERE g.task=outbox.task),
    'private:' || user
) WHERE state IN ('queued','sending','generating');

CREATE TRIGGER outbox_set_delivery_lane AFTER INSERT ON outbox
BEGIN
    UPDATE outbox SET delivery_lane=coalesce(
        (SELECT 'group:' || g.chat FROM telegram_group_tasks g WHERE g.task=NEW.task),
        'private:' || NEW.user
    ) WHERE id=NEW.id;
END;

CREATE INDEX outbox_delivery_lane_active ON outbox(delivery_lane,id)
    WHERE state IN ('queued','sending');
