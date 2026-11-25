use super::{McpApprovalScope, Pending, Task, requests};
use crate::{
    audit,
    mcp::{Connector, docblock},
    security::{id, now},
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Executor, Sqlite, SqlitePool};
use uuid::Uuid;

const PRIVATE: &str = "t.user=? AND t.chief_chat=1 AND t.agent IS NULL
    AND t.shared_memory_pair IS NULL AND t.state<>'revoked'
    AND EXISTS(SELECT 1 FROM users u WHERE u.id=t.user AND u.suspended_at IS NULL)
    AND NOT EXISTS(SELECT 1 FROM mail_messages m WHERE m.task=t.id)
    AND NOT EXISTS(SELECT 1 FROM telegram_group_tasks g WHERE g.task=t.id)";
const REVIEW_SECONDS: i64 = 900;
const PROJECTION: &str = "s.approval_task,s.request,s.source_event,s.path,s.url,
    CASE
        WHEN s.result='executing' AND p.state='executing' THEN 'saving'
        WHEN s.result='executing' THEN 'uncertain'
        WHEN s.result<>'ready' THEN s.result
        WHEN p.state='waiting' AND p.expires>=unixepoch() THEN 'review'
        WHEN p.state='approved' AND p.expires>=unixepoch() THEN 'queued'
        WHEN p.state IN ('denied','superseded') THEN p.state
        ELSE 'expired'
    END AS status";

#[derive(sqlx::FromRow, Serialize)]
pub(crate) struct SaveView {
    approval_task: String,
    request: String,
    source_event: i64,
    path: String,
    url: Option<String>,
    status: String,
}

pub(crate) struct Source<'a> {
    pub task: &'a str,
    pub revision: i64,
    pub event: i64,
}

pub(crate) struct Destination<'a> {
    pub connector: &'a Connector,
    pub workspace: &'a str,
    pub folder: &'a str,
    pub title: &'a str,
}

pub(crate) async fn available(db: &SqlitePool, user: &str, task: &str) -> Result<bool> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT EXISTS(SELECT 1 FROM tasks t WHERE {PRIVATE} AND t.id=?)"
    ))
    .bind(user)
    .bind(task)
    .fetch_one(db)
    .await?)
}

pub(crate) async fn list(db: &SqlitePool, user: &str, task: &str) -> Result<Vec<SaveView>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {PROJECTION} FROM docblock_task_saves s
         JOIN tasks t ON t.id=s.source LEFT JOIN pending p ON p.id=s.request
         WHERE {PRIVATE} AND s.source=? ORDER BY s.created DESC,s.approval_task LIMIT 12"
    ))
    .bind(user)
    .bind(task)
    .fetch_all(db)
    .await?)
}

async fn source_body<'a>(
    db: impl Executor<'a, Database = Sqlite>,
    user: &str,
    source: &Source<'_>,
) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT e.body FROM events e JOIN tasks t ON t.id=e.task
         WHERE {PRIVATE} AND t.id=? AND t.revision=? AND e.id=? AND e.id=(SELECT id FROM events
             WHERE task=t.id AND role='assistant' AND body<>'' ORDER BY id DESC LIMIT 1)"
    ))
    .bind(user)
    .bind(source.task)
    .bind(source.revision)
    .bind(source.event)
    .fetch_optional(db)
    .await?)
}

pub(crate) async fn create(
    db: &SqlitePool,
    user: &str,
    source: &Source<'_>,
    destination: &Destination<'_>,
) -> Result<SaveView> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let body = source_body(&mut *tx, user, source)
        .await?
        .context("The task result changed. Refresh before saving it.")?;
    ensure!(
        body.len() <= 24_000,
        "The reply is too large to review as one note."
    );
    let connector = destination.connector;
    let current: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM connectors WHERE id=? AND owner=? AND approval_epoch=? AND auth='oauth')",
    )
    .bind(&connector.id)
    .bind(user)
    .bind(connector.approval_epoch)
    .fetch_one(&mut *tx)
    .await?;
    ensure!(
        current && docblock::is_endpoint(&connector.endpoint),
        "Docblock connection changed."
    );

    let key = format!(
        "{:x}",
        Sha256::digest(
            json!([
                source.event,
                connector.id,
                destination.workspace,
                destination.folder,
                destination.title
            ])
            .to_string()
        )
    );
    let existing: Option<SaveView> = sqlx::query_as(&format!(
        "SELECT {PROJECTION} FROM docblock_task_saves s LEFT JOIN pending p ON p.id=s.request
         WHERE s.source=? AND s.destination_key=? AND (s.result IN ('executing','saved','proposed')
           OR (s.source_revision=? AND s.result='ready' AND p.state IN ('waiting','approved','executing')
               AND p.expires>=unixepoch()))
         ORDER BY s.created DESC LIMIT 1"
    ))
    .bind(source.task)
    .bind(&key)
    .bind(source.revision)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(existing) = existing {
        return Ok(existing);
    }

    let request = id();
    let approval_task = id();
    let suffix = Uuid::new_v4().simple().to_string();
    let slug: String = destination
        .title
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == ' ')
        .take(36)
        .collect::<String>()
        .to_ascii_lowercase();
    let slug = slug.split_whitespace().collect::<Vec<_>>().join("-");
    let slug = if slug.is_empty() { "note" } else { &slug };
    let folder = destination.folder;
    let path = if folder.is_empty() {
        format!("{slug}-{suffix}")
    } else {
        format!("{folder}/{slug}-{suffix}")
    };
    let args = json!({
        "workspace":destination.workspace,"path":path,"title":destination.title,
        "body":body,"reason":"Saved from a reviewed TinyBot task result",
    });
    let preview = docblock::review("write_note", &args)?;
    let scope = McpApprovalScope {
        connector: connector.id.clone(),
        epoch: connector.approval_epoch,
        tool: "write_note".into(),
    };
    let payload = json!({
        "call":{"name":"call_mcp","args":{"connector":connector.id,"tool":"write_note","arguments":args}},
        "mcp_approval":scope,"operation":preview["operation"],"destination":docblock::ORIGIN,"docblock":true,
    });
    let task: Task = sqlx::query_as(
        "INSERT INTO tasks(id,user,title,state,chief_chat,updated)
         VALUES(?,?,?,'running',1,?) RETURNING *",
    )
    .bind(&approval_task)
    .bind(user)
    .bind(format!("Save to Docblock · {}", destination.title))
    .bind(now())
    .fetch_one(&mut *tx)
    .await?;
    audit::open(&mut tx, &approval_task, "Save a task result to Docblock").await?;
    let change = preview["change"].as_str().unwrap_or("");
    let visible = crate::security::clip(change, 1600);
    let (notification, button) = if visible.len() == change.len() {
        (
            format!(
                "Save a private Docblock note\nWorkspace: {}\nNote: {path}\n\n{visible}",
                destination.workspace
            ),
            format!("mcp-approval-once/{request}"),
        )
    } else {
        (
            "Open Tasks to review the full Docblock note before saving.".into(),
            "mini".into(),
        )
    };
    requests::pause_in(
        &mut tx,
        &task,
        "approve",
        &payload,
        &notification,
        Some(&button),
        REVIEW_SECONDS,
        &request,
    )
    .await?;
    sqlx::query(
        "INSERT INTO docblock_task_saves
         (approval_task,request,source,source_revision,source_event,destination_key,path,created)
         VALUES(?,?,?,?,?,?,?,?)",
    )
    .bind(&approval_task)
    .bind(&request)
    .bind(source.task)
    .bind(source.revision)
    .bind(source.event)
    .bind(key)
    .bind(&path)
    .bind(now())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(SaveView {
        approval_task,
        request,
        source_event: source.event,
        path,
        url: None,
        status: "review".into(),
    })
}

pub(crate) async fn dedicated(db: &SqlitePool, task: &Task) -> Result<bool> {
    Ok(
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM docblock_task_saves WHERE approval_task=?)",
        )
        .bind(&task.id)
        .fetch_one(db)
        .await?,
    )
}

pub(crate) async fn source_current(
    db: &SqlitePool,
    task: &Task,
    request: &Pending,
) -> Result<bool> {
    let source: Option<(String, i64, i64)> = sqlx::query_as(
        "SELECT source,source_revision,source_event FROM docblock_task_saves
         WHERE approval_task=? AND request=? AND result='ready'",
    )
    .bind(&task.id)
    .bind(&request.id)
    .fetch_optional(db)
    .await?;
    let Some((id, revision, event)) = source else {
        return Ok(false);
    };
    Ok(source_body(
        db,
        &task.user,
        &Source {
            task: &id,
            revision,
            event,
        },
    )
    .await?
    .is_some())
}

pub(crate) async fn mark_executing(
    db: &SqlitePool,
    task: &Task,
    request: &Pending,
) -> Result<bool> {
    Ok(sqlx::query(&format!(
        "UPDATE docblock_task_saves SET result='executing' WHERE approval_task=? AND request=? AND result='ready'
         AND EXISTS(SELECT 1 FROM tasks t JOIN events e ON e.task=t.id
             WHERE {PRIVATE} AND t.id=docblock_task_saves.source AND t.revision=source_revision
               AND e.id=source_event AND e.id=(SELECT id FROM events
                   WHERE task=t.id AND role='assistant' AND body<>'' ORDER BY id DESC LIMIT 1))",
    ))
    .bind(&task.id)
    .bind(&request.id)
    .bind(&task.user)
    .execute(db)
    .await?
    .rows_affected() == 1)
}

pub(crate) async fn finish(
    db: &SqlitePool,
    task: &Task,
    request: &Pending,
    result: &str,
    url: Option<&str>,
    message: &str,
) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    requests::finish_action_in(&mut tx, task, &request.id, message, true).await?;
    sqlx::query(
        "UPDATE docblock_task_saves SET result=?,url=? WHERE approval_task=? AND request=?",
    )
    .bind(result)
    .bind(url)
    .bind(&task.id)
    .bind(&request.id)
    .execute(&mut *tx)
    .await?;
    settle(&mut tx, task).await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn dismiss(db: &SqlitePool, task: &Task) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    settle(&mut tx, task).await?;
    tx.commit().await?;
    Ok(())
}

async fn settle(tx: &mut sqlx::Transaction<'_, Sqlite>, task: &Task) -> Result<()> {
    sqlx::query("UPDATE tasks SET state='completed',updated=? WHERE id=? AND revision=? AND state='running'")
        .bind(now())
        .bind(&task.id)
        .bind(task.revision)
        .execute(&mut **tx)
        .await?;
    audit::mark_task(tx, task, "completed").await
}
