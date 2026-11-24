use super::{Outbound, Task};
use crate::security::{clip, id, now};
use anyhow::{Result, ensure};
use sqlx::SqlitePool;

#[derive(Clone, Copy, sqlx::FromRow)]
pub struct GroupRoute {
    pub chat: i64,
    pub topic: i64,
}

pub(crate) async fn request(
    db: &SqlitePool,
    operator: &str,
    user: &str,
    chat: i64,
) -> Result<bool> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let added = sqlx::query(
        "INSERT INTO telegram_groups(chat,requested_by,requested_at)
         SELECT ?,id,? FROM users WHERE id=? AND suspended_at IS NULL
         ON CONFLICT(chat) DO UPDATE SET requested_by=excluded.requested_by,
            requested_at=excluded.requested_at
         WHERE telegram_groups.enabled=0 AND telegram_groups.requested_at<?",
    )
    .bind(chat)
    .bind(now())
    .bind(user)
    .bind(now() - 86400)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if added {
        let message = format!(
            "A group administrator requested TinyBot access for Telegram chat {chat}. Send /approve_group {chat} here to enable group tasks."
        );
        sqlx::query("INSERT INTO outbox(user,body) VALUES(?,?)")
            .bind(operator)
            .bind(message)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(added)
}

pub(super) async fn set_enabled(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    chat: &str,
    enabled: bool,
) -> Result<bool> {
    let Ok(chat) = chat.parse::<i64>() else {
        return Ok(false);
    };
    let changed = sqlx::query(
        "UPDATE telegram_groups SET enabled=? WHERE chat=? AND enabled<>?
         AND (?=0 OR requested_at>=?)",
    )
    .bind(enabled)
    .bind(chat)
    .bind(enabled)
    .bind(enabled)
    .bind(now() - 86400)
    .execute(&mut **tx)
    .await?
    .rows_affected()
        == 1;
    if changed && !enabled {
        sqlx::query(
            "UPDATE tasks SET state='revoked',revision=revision+1 WHERE state<>'revoked'
             AND id IN (SELECT task FROM telegram_group_tasks WHERE chat=?)",
        )
        .bind(chat)
        .execute(&mut **tx)
        .await?;
        // A fresh administrator request is required before this group can be enabled again.
        sqlx::query("UPDATE telegram_groups SET requested_at=0 WHERE chat=?")
            .bind(chat)
            .execute(&mut **tx)
            .await?;
    }
    Ok(changed)
}

pub(crate) async fn input(
    db: &SqlitePool,
    user: &str,
    external: i64,
    route: GroupRoute,
    message: i64,
    reply: Option<i64>,
    text: &str,
) -> Result<bool> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let enabled: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM telegram_groups g JOIN users u ON u.id=?
                        WHERE g.chat=? AND g.enabled=1 AND u.suspended_at IS NULL AND u.provider IS NOT NULL)",
    ).bind(user).bind(route.chat).fetch_one(&mut *tx).await?;
    if !enabled {
        return Ok(false);
    }
    if sqlx::query("INSERT OR IGNORE INTO updates VALUES('telegram',?)")
        .bind(external)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 0
    {
        return Ok(false);
    }
    let text = clip(text.trim(), 8_000);
    if text.is_empty() {
        return Ok(false);
    }
    let task = if let Some(reply) = reply {
        let existing = sqlx::query_scalar::<_, String>(
            "SELECT t.id FROM outbox o JOIN telegram_group_tasks g ON g.task=o.task
             JOIN tasks t ON t.id=g.task
             WHERE o.user=? AND o.telegram_group_message_id=? AND o.state='sent' AND g.chat=? AND g.topic=?
                AND t.user=o.user AND t.state NOT IN ('cancelled','revoked')",
        ).bind(user).bind(reply).bind(route.chat).bind(route.topic)
        .fetch_optional(&mut *tx).await?;
        let Some(task) = existing else {
            return Ok(false);
        };
        task
    } else {
        let task = id();
        sqlx::query("INSERT INTO tasks(id,user,title,goal,updated,chief_chat) VALUES(?,?,?,?,?,0)")
            .bind(&task)
            .bind(user)
            .bind(clip(text, 100))
            .bind(clip(text, 1500))
            .bind(now())
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO telegram_group_tasks(task,chat,topic) VALUES(?,?,?)")
            .bind(&task)
            .bind(route.chat)
            .bind(route.topic)
            .execute(&mut *tx)
            .await?;
        task
    };
    sqlx::query(
        "UPDATE tasks SET revision=revision+1,updated=?,state=CASE WHEN state='running' THEN state ELSE 'queued' END
         WHERE id=?",
    ).bind(now()).bind(&task).execute(&mut *tx).await?;
    sqlx::query(
        "INSERT INTO events(task,role,body,telegram_message_id,telegram_reply_message_id)
         VALUES(?,'user',?,?,?)",
    )
    .bind(&task)
    .bind(text)
    .bind(message)
    .bind(reply.or(Some(message)))
    .execute(&mut *tx)
    .await?;
    crate::audit::open(&mut tx, &task, "Telegram group task").await?;
    tx.commit().await?;
    Ok(true)
}

pub(crate) async fn is_group_task(db: &SqlitePool, task: &str) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM telegram_group_tasks WHERE task=?)")
            .bind(task)
            .fetch_one(db)
            .await?,
    )
}

pub(crate) async fn current(db: &SqlitePool, task: &Task) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM telegram_group_tasks d JOIN telegram_groups g ON g.chat=d.chat
         JOIN tasks t ON t.id=d.task JOIN users u ON u.id=t.user
         WHERE t.id=? AND t.user=? AND t.revision=? AND t.state='running'
            AND t.chief_chat=0 AND g.enabled=1 AND u.suspended_at IS NULL)",
    )
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .fetch_one(db)
    .await?)
}

pub(crate) async fn destination(db: &SqlitePool, item: &Outbound) -> Result<Option<GroupRoute>> {
    let Some(task) = &item.task else {
        return Ok(None);
    };
    let assigned = is_group_task(db, task).await?;
    if !assigned {
        return Ok(None);
    }
    let route = sqlx::query_as(
        "SELECT d.chat,d.topic FROM telegram_group_tasks d JOIN telegram_groups g ON g.chat=d.chat
         JOIN tasks t ON t.id=d.task JOIN users u ON u.id=t.user
         WHERE t.id=? AND t.user=? AND t.chief_chat=0 AND t.state<>'revoked'
            AND g.enabled=1 AND u.suspended_at IS NULL",
    )
    .bind(task)
    .bind(&item.user)
    .fetch_optional(db)
    .await?;
    ensure!(route.is_some(), "Telegram group is unavailable");
    Ok(route)
}
