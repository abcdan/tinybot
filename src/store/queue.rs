use super::{Task, replies};
use crate::security::{clip, now};
use anyhow::Result;
use sqlx::{Executor, Sqlite, SqlitePool};

pub async fn list_tasks<'a>(
    db: impl Executor<'a, Database = Sqlite>,
    user: &str,
) -> Result<Vec<Task>> {
    Ok(sqlx::query_as(
        "SELECT t.* FROM tasks t WHERE t.user=? AND t.chief_chat=1 AND t.state<>'revoked'
         AND (t.agent IS NULL OR EXISTS(SELECT 1 FROM agents a WHERE a.id=t.agent
         AND (a.owner=t.user OR EXISTS(SELECT 1 FROM grants g WHERE g.agent=a.id AND g.member=t.user))))
         ORDER BY t.updated DESC, t.id LIMIT 40",
    )
    .bind(user)
    .fetch_all(db)
    .await?)
}

pub async fn select_task<'a>(
    db: impl Executor<'a, Database = Sqlite>,
    user: &str,
    task: &str,
) -> Result<bool> {
    let changed=sqlx::query("UPDATE users SET active_task=? WHERE id=? AND EXISTS(SELECT 1 FROM tasks t WHERE t.id=?
        AND t.user=users.id AND t.chief_chat=1 AND t.state NOT IN ('cancelled','revoked')
        AND (t.agent IS NULL OR EXISTS(SELECT 1 FROM agents a WHERE a.id=t.agent
        AND (a.owner=t.user OR EXISTS(SELECT 1 FROM grants g WHERE g.agent=a.id AND g.member=t.user)))))")
        .bind(task).bind(user).bind(task).execute(db).await?.rows_affected();
    Ok(changed == 1)
}

pub async fn recover(db: &SqlitePool) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("UPDATE tasks SET state='queued' WHERE state='running'")
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE outbox SET state=CASE WHEN reaction_event IS NULL THEN 'uncertain' ELSE 'queued' END
         WHERE state='sending'",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE pending SET state='lost' WHERE kind='form' AND state IN ('waiting','approved')",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE pending SET state=CASE WHEN kind IN ('approve','form-submit','email-send') THEN 'uncertain' ELSE 'interrupted' END
        WHERE state='executing'").execute(&mut *tx).await?;
    sqlx::query("UPDATE pending SET state='lost' WHERE kind='form-submit' AND state='approved'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO events(task,role,body) SELECT task,'tool',CASE WHEN state='uncertain' THEN
        'An external action may already have run. Tell the user and reconcile before trying it again.'
        ELSE 'The private form expired when TinyBot restarted. Request a fresh form if still needed.' END
        FROM pending WHERE state IN ('uncertain','lost') AND task IS NOT NULL")
        .execute(&mut *tx).await?;
    sqlx::query(
        "UPDATE tasks SET state='uncertain' WHERE state IN ('queued','waiting') AND id IN
        (SELECT task FROM pending WHERE state='uncertain')",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO outbox(user,task,agent,body) SELECT user,id,agent,
        'An approved action was interrupted. Check the destination, then send /reconcile to continue.'
        FROM tasks WHERE state='uncertain' AND id IN (SELECT task FROM pending WHERE state='uncertain')")
        .execute(&mut *tx).await?;
    sqlx::query(
        "UPDATE tasks SET state='queued' WHERE state='waiting' AND id IN
        (SELECT task FROM pending WHERE state='lost')",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE pending SET state='reconciled' WHERE state IN ('uncertain','lost')")
        .execute(&mut *tx)
        .await?;
    crate::audit::recover(&mut tx).await?;
    crate::audit::prune(&mut tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn expire(db: &SqlitePool) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    crate::audit::prune(&mut tx).await?;
    sqlx::query(
        "UPDATE pending SET state='expiring' WHERE expires<? AND state IN ('waiting','approved')",
    )
    .bind(now())
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO events(task,role,body) SELECT task,'tool','The pending request expired. Tell the user.'
        FROM pending WHERE state='expiring' AND task IS NOT NULL").execute(&mut *tx).await?;
    sqlx::query(
        "UPDATE tasks SET state='queued' WHERE state='waiting' AND id IN
        (SELECT task FROM pending WHERE state='expiring')",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE pending SET state='expired' WHERE state='expiring'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM invitations WHERE expires<?")
        .bind(now())
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM pending WHERE expires<? AND state<>'executing'")
        .bind(now() - 86400)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM suggested_replies WHERE expires<?")
        .bind(now())
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "DELETE FROM outbox WHERE id < (SELECT max(id)-10000 FROM outbox)
        AND state IN ('sent','uncertain','failed')
        AND NOT EXISTS(SELECT 1 FROM mail_messages m WHERE m.notice=outbox.id OR m.preview=outbox.id)",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn has_access(db: &SqlitePool, user: &str, agent: Option<&str>) -> Result<bool> {
    let Some(agent) = agent else { return Ok(true) };
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM agents a WHERE a.id=? AND
        (a.owner=? OR EXISTS(SELECT 1 FROM grants g WHERE g.agent=a.id AND g.member=?)))",
    )
    .bind(agent)
    .bind(user)
    .bind(user)
    .fetch_one(db)
    .await?)
}

pub async fn current(db: &SqlitePool, task: &Task) -> Result<bool> {
    let valid = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=? AND user=? AND revision=?
        AND state IN ('running','waiting'))",
    )
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .fetch_one(db)
    .await?;
    Ok(valid && has_access(db, &task.user, task.agent.as_deref()).await?)
}

pub async fn claim(db: &SqlitePool) -> Result<Option<Task>> {
    Ok(sqlx::query_as(
        "UPDATE tasks SET state='running' WHERE id=(SELECT id FROM tasks q WHERE state='queued'
        AND NOT EXISTS(SELECT 1 FROM tasks r WHERE r.user=q.user AND r.state='running')
        AND NOT EXISTS(SELECT 1 FROM pending p WHERE p.task=q.id AND p.state='executing')
        ORDER BY updated,id LIMIT 1) RETURNING *",
    )
    .fetch_optional(db)
    .await?)
}

pub async fn settle(
    db: &SqlitePool,
    task: &Task,
    state: &str,
    message: Option<&str>,
) -> Result<()> {
    settle_task(db, task, state, message, &[], None).await
}

pub async fn settle_reply(
    db: &SqlitePool,
    task: &Task,
    message: &str,
    suggestions: &[&str],
) -> Result<()> {
    settle_task(db, task, "idle", Some(message), suggestions, None).await
}

pub async fn settle_widget(
    db: &SqlitePool,
    task: &Task,
    card: &crate::channel::Card,
    suggestions: &[&str],
) -> Result<()> {
    let widget = serde_json::to_string(card)?;
    let message = card.plain_text();
    settle_task(db, task, "idle", Some(&message), suggestions, Some(&widget)).await
}

async fn settle_task(
    db: &SqlitePool,
    task: &Task,
    state: &str,
    message: Option<&str>,
    suggestions: &[&str],
    widget: Option<&str>,
) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let affected = sqlx::query(
        "UPDATE tasks SET state=CASE WHEN revision=? THEN ? ELSE 'queued' END,updated=?
        WHERE id=? AND state='running' RETURNING revision",
    )
    .bind(task.revision)
    .bind(state)
    .bind(now())
    .bind(&task.id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(row) = &affected {
        use sqlx::Row;
        if row.get::<i64, _>("revision") == task.revision {
            crate::audit::mark_task(&mut tx, task, state).await?;
        }
    }
    if let (Some(row), Some(message)) = (affected, message) {
        use sqlx::Row;
        if row.get::<i64, _>("revision") == task.revision {
            let assistant_event =
                sqlx::query("INSERT INTO events(task,role,body) VALUES(?,'assistant',?)")
                    .bind(&task.id)
                    .bind(message)
                    .execute(&mut *tx)
                    .await?
                    .last_insert_rowid();
            let outbox = sqlx::query(
                "INSERT INTO outbox(user,task,agent,body,widget,assistant_event_id,reply_to_message_id)
                 VALUES(?,?,?,?,?,?,(SELECT CASE WHEN telegram_reply_message_id IS NOT NULL
                    THEN COALESCE(telegram_message_id, telegram_reply_message_id) END
                    FROM events WHERE task=? AND role='user' ORDER BY id DESC LIMIT 1))",
            )
                    .bind(&task.user)
                    .bind(&task.id)
                    .bind(&task.agent)
                    .bind(message)
                    .bind(widget)
                    .bind(assistant_event)
                    .bind(&task.id)
                    .execute(&mut *tx)
                    .await?
                    .last_insert_rowid();
            replies::insert(&mut tx, outbox, task.revision, suggestions).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

pub async fn append(db: &SqlitePool, task: &str, role: &str, body: &str) -> Result<()> {
    sqlx::query("INSERT INTO events(task,role,body) VALUES(?,?,?)")
        .bind(task)
        .bind(role)
        .bind(clip(body, 12000))
        .execute(db)
        .await?;
    Ok(())
}

pub async fn history(db: &SqlitePool, task: &str) -> Result<Vec<(i64, String, String)>> {
    let mut rows =
        sqlx::query_as("SELECT id,role,body FROM events WHERE task=? ORDER BY id DESC LIMIT 180")
            .bind(task)
            .fetch_all(db)
            .await?;
    let rows: &mut Vec<(i64, String, String)> = &mut rows;
    rows.reverse();
    Ok(std::mem::take(rows))
}

pub async fn compaction_batch(
    db: &SqlitePool,
    task: &str,
    before: i64,
) -> Result<Vec<(i64, String, String)>> {
    let rows: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT id,role,body FROM events WHERE task=? AND id<?
         AND id IS NOT (SELECT max(id) FROM events WHERE task=? AND role='user')
        ORDER BY id LIMIT 28",
    )
    .bind(task)
    .bind(before)
    .bind(task)
    .fetch_all(db)
    .await?;
    let mut bytes = 0;
    Ok(rows
        .into_iter()
        .take_while(|(_, _, text)| {
            bytes += text.len();
            bytes <= 28000
        })
        .collect())
}

pub async fn compact(
    db: &SqlitePool,
    task: &Task,
    boundary: i64,
    summary: &str,
) -> Result<Option<String>> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let changed =
        sqlx::query("UPDATE tasks SET summary=? WHERE id=? AND revision=? AND state='running'")
            .bind(summary)
            .bind(&task.id)
            .bind(task.revision)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    if changed == 0 {
        return Ok(None);
    }
    sqlx::query(
        "DELETE FROM events WHERE task=? AND id<=?
         AND id IS NOT (SELECT max(id) FROM events WHERE task=? AND role='user')",
    )
    .bind(&task.id)
    .bind(boundary)
    .bind(&task.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(summary.into()))
}
