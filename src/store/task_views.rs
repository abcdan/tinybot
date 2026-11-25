use super::{Pending, ReplyContext, Task, checkpoints, queue, requests};
use crate::{
    audit,
    runtime::confirmed_step,
    security::{clip, now},
};
use anyhow::Result;
use serde::Serialize;
use sqlx::{Executor, Sqlite, SqlitePool, Transaction};

const VISIBLE: &str = "t.user=? AND t.chief_chat=1 AND t.state<>'revoked'
    AND EXISTS(SELECT 1 FROM users u WHERE u.id=t.user AND u.suspended_at IS NULL)
    AND (t.shared_memory_pair IS NULL OR EXISTS(SELECT 1 FROM available_shared_memory_pairs p
      WHERE p.id=t.shared_memory_pair AND (p.owner=t.user OR p.guest=t.user)))
    AND (t.agent IS NULL OR EXISTS(SELECT 1 FROM agents a WHERE a.id=t.agent
      AND (a.owner=t.user OR EXISTS(SELECT 1 FROM grants g WHERE g.agent=a.id AND g.member=t.user))))";
const FIELDS: &str = "t.id,t.title,t.state,t.revision,t.updated,t.agent";

pub(crate) fn completable(state: &str) -> bool {
    matches!(
        state,
        "idle" | "blocked" | "queued" | "running" | "waiting" | "setup"
    )
}

#[derive(sqlx::FromRow, Serialize)]
pub(crate) struct TaskView {
    id: String,
    title: String,
    state: String,
    revision: i64,
    updated: i64,
    agent: Option<String>,
}

#[derive(sqlx::FromRow, Serialize)]
pub(crate) struct AuditSummary {
    id: i64,
    started_at: i64,
    finished_at: Option<i64>,
    status: String,
}

#[derive(Serialize)]
pub(crate) struct ReplyPreview {
    event: i64,
    text: String,
    truncated: bool,
}

#[derive(Serialize)]
pub(crate) struct TaskDetail {
    task: TaskView,
    audit: Option<AuditSummary>,
    reply: Option<ReplyPreview>,
    last_confirmed: Option<&'static str>,
    failure: Option<String>,
    executing: bool,
    plan: Vec<checkpoints::Step>,
    #[serde(skip)]
    pub(crate) requests: Vec<Pending>,
}

pub(crate) async fn recent(db: &SqlitePool, user: &str) -> Result<Vec<TaskView>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {FIELDS} FROM tasks t WHERE {VISIBLE}
         ORDER BY t.state IN ('completed','cancelled'),t.updated DESC,t.id LIMIT 40"
    ))
    .bind(user)
    .fetch_all(db)
    .await?)
}

pub(crate) async fn detail(db: &SqlitePool, user: &str, id: &str) -> Result<Option<TaskDetail>> {
    let mut tx = db.begin().await?;
    let task: Option<TaskView> = sqlx::query_as(&format!(
        "SELECT {FIELDS} FROM tasks t WHERE {VISIBLE} AND t.id=?"
    ))
    .bind(user)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(task) = task else {
        return Ok(None);
    };
    let audit: Option<AuditSummary> = sqlx::query_as(
        "SELECT s.id,s.started_at,s.finished_at,s.status FROM tasks t
         JOIN audit_sessions s ON s.id=t.audit_session_id AND s.task=t.id
         WHERE t.id=? AND s.started_at>?",
    )
    .bind(id)
    .bind(now() - audit::RETENTION)
    .fetch_optional(&mut *tx)
    .await?;
    let last_confirmed = if let Some(session) = &audit {
        let steps: Vec<(String, String)> = sqlx::query_as(
            "SELECT name,detail FROM audit_entries WHERE session=? AND kind='tool' AND status='completed'
             ORDER BY id DESC LIMIT 32",
        )
        .bind(session.id)
        .fetch_all(&mut *tx)
        .await?;
        steps.iter().find_map(|(name, raw)| {
            let detail = serde_json::from_str(raw).ok()?;
            confirmed_step(name, &detail)
        })
    } else {
        None
    };
    let reply: Option<(i64, String)> = sqlx::query_as(
        "SELECT id,body FROM events WHERE task=? AND role='assistant' AND body<>'' ORDER BY id DESC LIMIT 1",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let reply = reply.map(|(event, body)| {
        let text = clip(&body, 8_000);
        ReplyPreview {
            event,
            truncated: text.len() < body.len(),
            text: text.into(),
        }
    });
    let executing = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pending WHERE task=? AND user=? AND state='executing')",
    )
    .bind(id)
    .bind(user)
    .fetch_one(&mut *tx)
    .await?;
    let failure = if task.state == "blocked" {
        sqlx::query_scalar("SELECT message FROM task_failures WHERE task=? AND revision=?")
            .bind(id)
            .bind(task.revision)
            .fetch_optional(&mut *tx)
            .await?
    } else {
        None
    };
    let requests = requests::list_requests_in(&mut *tx, user, Some(id)).await?;
    let plan = checkpoints::plan(&mut *tx, id, task.revision, task.state == "completed").await?;
    tx.commit().await?;
    Ok(Some(TaskDetail {
        task,
        audit,
        reply,
        last_confirmed,
        failure,
        executing,
        plan,
        requests,
    }))
}

pub(crate) async fn search(
    db: &SqlitePool,
    user: &str,
    query: &str,
    offset: i64,
) -> Result<Vec<TaskView>> {
    search_in_scope(db, user, query, offset, false).await
}

pub(crate) async fn search_in_scope(
    db: &SqlitePool,
    user: &str,
    query: &str,
    offset: i64,
    completed_only: bool,
) -> Result<Vec<TaskView>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {FIELDS} FROM tasks t WHERE {VISIBLE}
         AND (?=0 OR t.state='completed')
         AND (?='' OR instr(lower(t.title),lower(?))>0 OR instr(lower(t.summary),lower(?))>0
         OR EXISTS(SELECT 1 FROM events e WHERE e.task=t.id AND e.role IN ('user','assistant')
                  AND instr(lower(e.body),lower(?))>0))
         ORDER BY t.updated DESC,t.id LIMIT 21 OFFSET ?"
    ))
    .bind(user)
    .bind(completed_only)
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
    Ok(match reply {
        Some(reply) if reply.from_bot => {
            sqlx::query_scalar::<_, String>(
                "SELECT task FROM outbox WHERE user=? AND telegram_message_id=?
             AND state='sent' AND task IS NOT NULL",
            )
            .bind(user)
            .bind(reply.message_id)
            .fetch_optional(&mut **tx)
            .await?
        }
        Some(reply) => {
            sqlx::query_scalar::<_, String>(
                "SELECT e.task FROM events e JOIN tasks t ON t.id=e.task
             WHERE t.user=? AND t.chief_chat=1 AND e.telegram_message_id=? AND e.role='user'
             ORDER BY e.id DESC LIMIT 1",
            )
            .bind(user)
            .bind(reply.message_id)
            .fetch_optional(&mut **tx)
            .await?
        }
        None => active.map(str::to_owned),
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
        return queue_status(tx, &task, executing).await;
    }
    sqlx::query(
        "INSERT INTO outbox(user,body) VALUES(?,'No task is selected. Use /tasks to find a task.')",
    )
    .bind(user)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn label(state: &str) -> &str {
    match state {
        "queued" => "Queued",
        "running" => "Working",
        "waiting" => "Waiting for your response",
        "setup" => "Waiting for model setup",
        "uncertain" => "Check the destination, then use /reconcile",
        "idle" => "Ready for a follow-up",
        "completed" => "Completed",
        "blocked" => "Stopped with an error",
        "cancelled" => "Stopped",
        _ => "Unavailable",
    }
}

async fn queue_status(
    tx: &mut Transaction<'_, Sqlite>,
    task: &Task,
    executing: bool,
) -> Result<()> {
    let state = if executing {
        "An approved action is in progress"
    } else {
        label(&task.state)
    };
    let controls = if executing { "executing" } else { &task.state };
    let button = format!("task/{}/{}/{controls}", task.id, task.revision);
    let mut body = format!("{}\n{state}", task.title);
    for step in checkpoints::plan(
        &mut **tx,
        &task.id,
        task.revision,
        task.state == "completed",
    )
    .await?
    {
        let mark = if step.completed_at.is_some() {
            "✓"
        } else {
            "○"
        };
        body.push_str(&format!("\n{mark} {}", step.title));
    }
    sqlx::query("INSERT INTO outbox(user,task,agent,body,button) VALUES(?,?,?,?,?)")
        .bind(&task.user)
        .bind(&task.id)
        .bind(&task.agent)
        .bind(body)
        .bind(button)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub(crate) async fn stop(db: &SqlitePool, user: &str, task: &str, revision: i64) -> Result<bool> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let result = stop_current(&mut tx, user, task, revision).await?;
    tx.commit().await?;
    Ok(result)
}

pub(crate) async fn select_at(
    db: &SqlitePool,
    user: &str,
    task: &str,
    revision: i64,
) -> Result<bool> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let selected = visible(&mut *tx, user, task)
        .await?
        .is_some_and(|task| task.revision == revision)
        && queue::select_task_in(&mut tx, user, task).await?;
    tx.commit().await?;
    Ok(selected)
}

pub(crate) async fn complete(
    db: &SqlitePool,
    user: &str,
    task: &str,
    revision: i64,
) -> Result<bool> {
    complete_with_reply(db, user, task, revision, None).await
}

pub(crate) async fn complete_with_reply(
    db: &SqlitePool,
    user: &str,
    task: &str,
    revision: i64,
    reply: Option<&str>,
) -> Result<bool> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let Some(current) = visible(&mut *tx, user, task).await? else {
        return Ok(false);
    };
    let changed = current.revision == revision && complete_current(&mut tx, &current).await?;
    if changed && let Some(reply) = reply.map(str::trim).filter(|text| !text.is_empty()) {
        let reply = clip(reply, 800);
        let event = sqlx::query("INSERT INTO events(task,role,body) VALUES(?,'assistant',?)")
            .bind(task)
            .bind(reply)
            .execute(&mut *tx)
            .await?
            .last_insert_rowid();
        sqlx::query(
            "INSERT INTO outbox(user,task,agent,body,assistant_event_id,reply_to_message_id)
             VALUES(?,?,?,?,?,(SELECT telegram_message_id FROM events WHERE task=? AND role='user'
                               ORDER BY id DESC LIMIT 1))",
        )
        .bind(user)
        .bind(task)
        .bind(&current.agent)
        .bind(reply)
        .bind(event)
        .bind(task)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(changed)
}

pub(super) async fn complete_current(
    tx: &mut Transaction<'_, Sqlite>,
    task: &Task,
) -> Result<bool> {
    if !completable(&task.state)
        || sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM pending WHERE task=? AND state='executing')",
        )
        .bind(&task.id)
        .fetch_one(&mut **tx)
        .await?
    {
        return Ok(false);
    }
    audit::task_completed(tx, task).await?;
    sqlx::query("UPDATE tasks SET state='completed',revision=revision+1,updated=? WHERE id=?")
        .bind(now())
        .bind(&task.id)
        .execute(&mut **tx)
        .await?;
    clear_requests_and_selection(tx, &task.user, &task.id).await?;
    Ok(true)
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
    clear_requests_and_selection(tx, user, task).await?;
    Ok(true)
}

async fn clear_requests_and_selection(
    tx: &mut Transaction<'_, Sqlite>,
    user: &str,
    task: &str,
) -> Result<()> {
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
    Ok(())
}
