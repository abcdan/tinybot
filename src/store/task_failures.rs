use super::{Outbound, ReplyContext, Task, task_views};
use crate::{
    audit,
    security::{id, now},
};
use anyhow::Result;
use sqlx::{Sqlite, SqlitePool, Transaction};

const LEGACY_ERROR: &str =
    "I couldn’t finish this turn. Your task is saved. Send a follow-up to try again.";
pub(crate) const RETRY: &str = "retry-turn/";

pub(crate) async fn record(
    db: &SqlitePool,
    task: &Task,
    message: &str,
    retryable: bool,
) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let live: Option<i64> =
        sqlx::query_scalar("SELECT revision FROM tasks WHERE id=? AND user=? AND state='running'")
            .bind(&task.id)
            .bind(&task.user)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(live) = live else {
        return Ok(());
    };
    let executing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM pending WHERE task=? AND user=? AND state='executing'
         AND kind IN ('approve','form-submit','email-send') LIMIT 1",
    )
    .bind(&task.id)
    .bind(&task.user)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(action) = executing {
        tx.rollback().await?;
        return super::uncertain_action(
            db, task, &action,
            "I lost confirmation of an approved action. Check its destination, then send /reconcile to continue.",
        ).await;
    }
    sqlx::query("UPDATE pending SET state='interrupted' WHERE task=? AND state='executing'")
        .bind(&task.id)
        .execute(&mut *tx)
        .await?;
    if live != task.revision {
        sqlx::query("UPDATE tasks SET state='queued' WHERE id=?")
            .bind(&task.id)
            .execute(&mut *tx)
            .await?;
    } else {
        sqlx::query("UPDATE tasks SET state='blocked',updated=? WHERE id=?")
            .bind(now())
            .bind(&task.id)
            .execute(&mut *tx)
            .await?;
        audit::mark_task(&mut tx, task, "blocked").await?;
        let token = id();
        let retryable = retryable && can_retry(&mut tx, task).await?;
        sqlx::query(
            "INSERT INTO task_failures(task,token,revision,message,retryable) VALUES(?,?,?,?,?)",
        )
        .bind(&task.id)
        .bind(&token)
        .bind(task.revision)
        .bind(message)
        .bind(retryable)
        .execute(&mut *tx)
        .await?;
        queue_notice(&mut tx, task, &token).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub(super) async fn status(tx: &mut Transaction<'_, Sqlite>, task: &Task) -> Result<bool> {
    let token: Option<String> = sqlx::query_scalar(
        "SELECT token FROM task_failures WHERE task=? AND revision=? AND ?='blocked'",
    )
    .bind(&task.id)
    .bind(task.revision)
    .bind(&task.state)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(token) = token else {
        return Ok(false);
    };
    queue_notice(tx, task, &token).await?;
    Ok(true)
}

async fn queue_notice(tx: &mut Transaction<'_, Sqlite>, task: &Task, token: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO outbox(user,task,agent,body,button,reply_to_message_id)
         VALUES(?,?,?,'',?,(SELECT e.telegram_message_id FROM events e
            WHERE e.task=? AND e.role='user' ORDER BY e.id DESC LIMIT 1))",
    )
    .bind(&task.user)
    .bind(&task.id)
    .bind(&task.agent)
    .bind(format!("{RETRY}{token}"))
    .bind(&task.id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn notice(
    db: &SqlitePool,
    item: &Outbound,
    token: &str,
) -> Result<Option<(String, bool)>> {
    Ok(sqlx::query_as(
        "SELECT f.message,f.retryable FROM task_failures f JOIN tasks t ON t.id=f.task AND t.revision=f.revision
         WHERE f.token=? AND t.id IS ? AND t.user=? AND t.state='blocked'",
    ).bind(token).bind(&item.task).bind(&item.user).fetch_optional(db).await?)
}

pub(crate) async fn retry(db: &SqlitePool, user: &str, token: &str) -> Result<bool> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let task: Option<String> =
        sqlx::query_scalar("SELECT task FROM task_failures WHERE token=? AND retryable=1")
            .bind(token)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(task) = task else {
        return Ok(false);
    };
    let queued = retry_task(&mut tx, user, &task, false).await?;
    tx.commit().await?;
    Ok(queued)
}

pub(super) async fn retry_active(
    tx: &mut Transaction<'_, Sqlite>,
    user: &str,
    active: Option<&str>,
    reply: Option<&ReplyContext>,
) -> Result<bool> {
    let task = task_views::resolve(tx, user, active, reply).await?;
    let Some(task) = task else {
        return Ok(false);
    };
    retry_task(tx, user, &task, true).await
}

async fn retry_task(
    tx: &mut Transaction<'_, Sqlite>,
    user: &str,
    id: &str,
    allow_legacy: bool,
) -> Result<bool> {
    let Some(task) = task_views::visible(&mut **tx, user, id).await? else {
        return Ok(false);
    };
    if task.state != "blocked" || !can_retry(tx, &task).await? {
        return Ok(false);
    }
    let eligible: bool = sqlx::query_scalar(
        "SELECT coalesce(
          (SELECT (retryable OR ?3) FROM task_failures WHERE task=?1 AND revision=?2),
          (?3 AND coalesce((SELECT role='assistant' AND body=?4 FROM events
                            WHERE task=?1 ORDER BY id DESC LIMIT 1),0)))",
    )
    .bind(id)
    .bind(task.revision)
    .bind(allow_legacy)
    .bind(LEGACY_ERROR)
    .fetch_one(&mut **tx)
    .await?;
    if !eligible {
        return Ok(false);
    }
    sqlx::query("UPDATE tasks SET state='queued',updated=? WHERE id=?")
        .bind(now())
        .bind(id)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "INSERT INTO outbox(user,task,agent,body) VALUES(?,?,?,'I’ll retry your saved message.')",
    )
    .bind(user)
    .bind(id)
    .bind(task.agent)
    .execute(&mut **tx)
    .await?;
    Ok(true)
}

async fn can_retry(tx: &mut Transaction<'_, Sqlite>, task: &Task) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT NOT EXISTS(SELECT 1 FROM task_tool_attempts WHERE task=?1 AND revision=?2)
             AND NOT EXISTS(SELECT 1 FROM pending WHERE task=?1 AND revision=?2)
             AND EXISTS(SELECT 1 FROM events WHERE task=?1 AND role='user')",
    )
    .bind(&task.id)
    .bind(task.revision)
    .fetch_one(&mut **tx)
    .await?)
}
