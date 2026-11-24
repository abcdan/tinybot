use super::{ReplyContext, Task};
use crate::{audit, security::now};
use anyhow::Result;
use serde::Serialize;
use sqlx::{Executor, Sqlite, SqlitePool, Transaction};

const VISIBLE: &str = "t.user=? AND t.chief_chat=1 AND t.state<>'revoked'
    AND EXISTS(SELECT 1 FROM users u WHERE u.id=t.user AND u.suspended_at IS NULL)
    AND (t.shared_memory_pair IS NULL OR EXISTS(SELECT 1 FROM available_shared_memory_pairs p
      WHERE p.id=t.shared_memory_pair AND (p.owner=t.user OR p.guest=t.user)))
    AND (t.agent IS NULL OR EXISTS(SELECT 1 FROM agents a WHERE a.id=t.agent
      AND (a.owner=t.user OR EXISTS(SELECT 1 FROM grants g WHERE g.agent=a.id AND g.member=t.user))))";

#[derive(sqlx::FromRow, Serialize)]
pub(crate) struct TaskView {
    id: String,
    title: String,
    state: String,
    revision: i64,
}

pub(crate) async fn search(
    db: &SqlitePool,
    user: &str,
    query: &str,
    offset: i64,
) -> Result<Vec<TaskView>> {
    Ok(sqlx::query_as(&format!(
        "SELECT t.id,t.title,t.state,t.revision FROM tasks t WHERE {VISIBLE}
         AND (?='' OR instr(lower(t.title),lower(?))>0 OR instr(lower(t.summary),lower(?))>0
         OR EXISTS(SELECT 1 FROM events e WHERE e.task=t.id AND e.role IN ('user','assistant')
                  AND instr(lower(e.body),lower(?))>0))
         ORDER BY t.updated DESC,t.id LIMIT 21 OFFSET ?"
    ))
    .bind(user)
    .bind(query)
    .bind(query)
    .bind(query)
    .bind(query)
    .bind(offset)
    .fetch_all(db)
    .await?)
}

pub(super) async fn visible<'a>(
    db: impl Executor<'a, Database = Sqlite>,
    user: &str,
    task: &str,
) -> Result<Option<Task>> {
    Ok(sqlx::query_as(&format!(
        "SELECT t.* FROM tasks t WHERE {VISIBLE} AND t.id=?"
    ))
    .bind(user)
    .bind(task)
    .fetch_optional(db)
    .await?)
}

pub(super) async fn resolve(
    tx: &mut Transaction<'_, Sqlite>,
    user: &str,
    active: Option<&str>,
    reply: Option<&ReplyContext>,
) -> Result<Option<String>> {
    Ok(if let Some(reply) = reply.filter(|reply| reply.from_bot) {
        sqlx::query_scalar::<_, String>(
            "SELECT task FROM outbox WHERE user=? AND telegram_message_id=?
             AND state='sent' AND task IS NOT NULL",
        )
        .bind(user)
        .bind(reply.message_id)
        .fetch_optional(&mut **tx)
        .await?
    } else {
        active.map(str::to_owned)
    })
}

pub(super) async fn status(
    tx: &mut Transaction<'_, Sqlite>,
    user: &str,
    active: Option<&str>,
    reply: Option<&ReplyContext>,
) -> Result<()> {
    let target = resolve(tx, user, active, reply).await?;
    if let Some(target) = target
        && let Some(task) = visible(&mut **tx, user, &target).await?
    {
        if super::task_failures::status(tx, &task).await? {
            return Ok(());
        }
        let executing = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM pending WHERE task=? AND user=? AND state='executing'
                           AND kind IN ('approve','form-submit','email-send'))",
        )
        .bind(&task.id)
        .bind(user)
        .fetch_one(&mut **tx)
        .await?;
        return queue_status(tx, &task, executing, false).await;
    }
    sqlx::query(
        "INSERT INTO outbox(user,body) VALUES(?,'No task is selected. Use /tasks to find a task.')",
    )
    .bind(user)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(super) async fn notify_if_queued(
    tx: &mut Transaction<'_, Sqlite>,
    user: &str,
    id: &str,
    was_queued: bool,
) -> Result<()> {
    let Some(task) = visible(&mut **tx, user, id).await? else {
        return Ok(());
    };
    if task.state != "queued" {
        return Ok(());
    }
    if was_queued {
        sqlx::query(
            "UPDATE outbox SET button=? WHERE task=? AND state='queued' AND button LIKE 'tq/%'",
        )
        .bind(format!("tq/{}/{}/queued", task.id, task.revision))
        .bind(&task.id)
        .execute(&mut **tx)
        .await?;
        return Ok(());
    }
    let other_running = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM tasks WHERE user=? AND id<>? AND state='running')",
    )
    .bind(user)
    .bind(id)
    .fetch_one(&mut **tx)
    .await?;
    if other_running {
        queue_status(tx, &task, false, true).await?;
    }
    Ok(())
}

pub(super) async fn acknowledge_input(
    tx: &mut Transaction<'_, Sqlite>,
    user: &str,
    id: &str,
    message: i64,
    already_working: bool,
) -> Result<()> {
    let Some(task) = visible(&mut **tx, user, id).await? else {
        return Ok(());
    };
    if !matches!(task.state.as_str(), "queued" | "running") {
        return Ok(());
    }
    let button = format!("ta/{id}/{}", task.revision);
    if already_working {
        sqlx::query(
            "UPDATE outbox SET button=? WHERE user=? AND task=?
             AND state IN ('queued','sending') AND (button LIKE 'ta/%' OR button LIKE 'tq/%')",
        )
        .bind(&button)
        .bind(user)
        .bind(id)
        .execute(&mut **tx)
        .await?;
        return Ok(());
    }
    let waiting: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM tasks q JOIN tasks t ON t.id=?
         WHERE q.user=t.user AND q.id<>t.id AND (q.state='running' OR
            (q.state='queued' AND (q.updated,q.id)<(t.updated,t.id)
             AND NOT EXISTS(SELECT 1 FROM pending p WHERE p.task=q.id AND p.state='executing'))))",
    )
    .bind(id)
    .fetch_one(&mut **tx)
    .await?;
    let body = if waiting {
        "Got it. I’ve queued your task behind your other work."
    } else {
        "Got it. I’ll get to work."
    };
    sqlx::query(
        "UPDATE events SET acknowledged=1 WHERE task=? AND role='user' AND telegram_message_id=?",
    )
    .bind(id)
    .bind(message)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO outbox(user,task,agent,body,button,reply_to_message_id) VALUES(?,?,?,?,?,?)",
    )
    .bind(user)
    .bind(id)
    .bind(&task.agent)
    .bind(body)
    .bind(button)
    .bind(message)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn acknowledgement_controls(
    db: &SqlitePool,
    user: &str,
    id: &str,
    outbox: i64,
) -> Result<Option<String>> {
    let current: Option<(i64, String)> = sqlx::query_as(&format!(
        "SELECT t.revision,t.state FROM tasks t JOIN outbox o ON o.task=t.id AND o.user=t.user
         WHERE {VISIBLE} AND t.id=? AND o.id=? AND o.state='sending'
         AND o.button='ta/' || t.id || '/' || t.revision
         AND t.state IN ('queued','running','waiting','idle')
         AND NOT EXISTS(SELECT 1 FROM outbox r WHERE r.task=t.id AND r.id>o.id
                        AND r.state='sent' AND r.assistant_event_id IS NOT NULL)"
    ))
    .bind(user)
    .bind(id)
    .bind(outbox)
    .fetch_optional(db)
    .await?;
    Ok(current.map(|(revision, state)| format!("task/{id}/{revision}/{state}")))
}

fn label(state: &str) -> &str {
    match state {
        "queued" => "Queued behind another task",
        "running" => "Working",
        "waiting" => "Waiting for your response",
        "setup" => "Waiting for model setup",
        "uncertain" => "Check the destination, then use /reconcile",
        "idle" => "Ready for a follow-up",
        "blocked" => "Stopped with an error",
        "cancelled" => "Stopped",
        _ => "Unavailable",
    }
}

async fn queue_status(
    tx: &mut Transaction<'_, Sqlite>,
    task: &Task,
    executing: bool,
    notice: bool,
) -> Result<()> {
    let prefix = if notice { "tq" } else { "task" };
    let state = if executing {
        "An approved action is in progress"
    } else if !notice && task.state == "queued" {
        "Queued"
    } else {
        label(&task.state)
    };
    let controls = if executing { "executing" } else { &task.state };
    let button = format!("{prefix}/{}/{}/{controls}", task.id, task.revision);
    sqlx::query("INSERT INTO outbox(user,task,agent,body,button) VALUES(?,?,?,?,?)")
        .bind(&task.user)
        .bind(&task.id)
        .bind(&task.agent)
        .bind(format!("{}\n{state}", task.title))
        .bind(button)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub(crate) async fn queued_notice_is_current(
    db: &SqlitePool,
    user: &str,
    id: &str,
    revision: i64,
) -> Result<bool> {
    Ok(visible(db, user, id)
        .await?
        .is_some_and(|task| task.revision == revision && task.state == "queued"))
}

pub(crate) async fn stop(db: &SqlitePool, user: &str, task: &str, revision: i64) -> Result<bool> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let result = stop_current(&mut tx, user, task, revision).await?;
    tx.commit().await?;
    Ok(result)
}

pub(super) async fn stop_current(
    tx: &mut Transaction<'_, Sqlite>,
    user: &str,
    task: &str,
    revision: i64,
) -> Result<bool> {
    let Some(current) = visible(&mut **tx, user, task).await? else {
        return Ok(false);
    };
    if current.revision != revision
        || !matches!(
            current.state.as_str(),
            "queued" | "running" | "waiting" | "setup"
        )
    {
        return Ok(false);
    }
    // An already-started write owns the reconciliation path until it settles.
    if sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM pending WHERE task=? AND state='executing'
                       AND kind IN ('approve','form-submit','email-send'))",
    )
    .bind(task)
    .fetch_one(&mut **tx)
    .await?
    {
        return Ok(false);
    }
    sqlx::query("UPDATE tasks SET state='cancelled',revision=revision+1,updated=? WHERE id=?")
        .bind(now())
        .bind(task)
        .execute(&mut **tx)
        .await?;
    audit::cancel(tx, task, user).await?;
    sqlx::query(
        "UPDATE pending SET state='superseded' WHERE task=? AND state IN ('waiting','approved')",
    )
    .bind(task)
    .execute(&mut **tx)
    .await?;
    sqlx::query("UPDATE users SET active_task=NULL WHERE id=? AND active_task=?")
        .bind(user)
        .bind(task)
        .execute(&mut **tx)
        .await?;
    Ok(true)
}
