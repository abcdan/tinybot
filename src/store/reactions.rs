use super::{Outbound, Task};
use anyhow::{Result, ensure};
use sqlx::SqlitePool;

pub const REACTION_EMOJIS: &[&str] = &["👍", "❤", "🔥", "🎉", "👀", "👏", "🙏", "🤔", "😁", "🫡"];

pub async fn queue_reaction(db: &SqlitePool, task: &Task, emoji: &str) -> Result<bool> {
    let emoji = if emoji == "❤️" { "❤" } else { emoji };
    ensure!(
        REACTION_EMOJIS.contains(&emoji),
        "unsupported Telegram reaction"
    );

    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let event: Option<i64> = sqlx::query_scalar(
        "UPDATE events SET telegram_reaction_queued=1
        WHERE id=(SELECT e.id FROM events e JOIN tasks t ON t.id=e.task
            WHERE t.id=? AND t.user=? AND t.revision=? AND t.state='running' AND e.role='user'
            AND (t.agent IS NULL OR EXISTS(SELECT 1 FROM agents a WHERE a.id=t.agent
                AND (a.owner=t.user OR EXISTS(SELECT 1 FROM grants g WHERE g.agent=a.id AND g.member=t.user))))
            ORDER BY e.id DESC LIMIT 1)
        AND telegram_message_id>0 AND telegram_reaction_queued=0 RETURNING id",
    )
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(event) = event {
        sqlx::query(
            "INSERT INTO outbox(user,task,agent,body,reaction_event)
            SELECT user,id,agent,?,? FROM tasks WHERE id=?",
        )
        .bind(emoji)
        .bind(event)
        .bind(&task.id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(event.is_some())
}

pub async fn reaction_target(db: &SqlitePool, item: &Outbound) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT e.telegram_message_id FROM events e JOIN tasks t ON t.id=e.task
        WHERE e.id=? AND e.role='user' AND e.telegram_message_id>0 AND t.id=? AND t.user=?
        AND t.state NOT IN ('cancelled','revoked')
        AND e.id=(SELECT id FROM events WHERE task=t.id AND role='user' ORDER BY id DESC LIMIT 1)",
    )
    .bind(item.reaction_event)
    .bind(&item.task)
    .bind(&item.user)
    .fetch_optional(db)
    .await?)
}

pub(crate) async fn has_queued_reaction(db: &SqlitePool, task: &Task) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM tasks t JOIN events e ON e.id=(
            SELECT max(id) FROM events WHERE task=t.id AND role='user')
         JOIN outbox o ON o.reaction_event=e.id AND o.task=t.id AND o.user=t.user
         WHERE t.id=? AND t.user=? AND t.revision=? AND t.state='running'
           AND e.telegram_reaction_queued=1 AND o.state IN ('queued','sending','sent'))",
    )
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .fetch_one(db)
    .await?)
}
