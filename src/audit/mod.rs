mod budget;
mod delivery;
mod details;
mod usage;

pub(crate) use budget::{alert_usage, set_alert};
pub(crate) use delivery::{draft_delivered, handoff_queued};
pub(crate) use usage::{ModelCall, ingress_model_call, provider_call};

pub(crate) fn request_preview(text: &str) -> String {
    details::message_preview(text)
}

use crate::{model::provider_failure, runtime::Outcome, security::now, store::Task};
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};
use std::{future::Future, time::Instant};

pub(crate) const RETENTION: i64 = 7 * 24 * 60 * 60;
const SESSION_PAGE: usize = 20;
const ENTRY_PAGE: usize = 100;

tokio::task_local! {
    static CURRENT: Option<Trace>;
    static MODEL_SCOPE: &'static str;
}

#[derive(Clone)]
pub(crate) struct Trace {
    db: SqlitePool,
    session: i64,
    attempt: i64,
    revision: i64,
}

#[derive(Default, Serialize)]
pub(crate) struct ProgressEvidence {
    pub completed_steps: usize,
    pub searches_with_results: usize,
    pub pages_read: usize,
}

impl Trace {
    pub(crate) async fn for_progress(
        db: &SqlitePool,
        task: &str,
        user: &str,
        revision: i64,
    ) -> Result<Option<Self>> {
        let scope: Option<(i64, i64)> = sqlx::query_as(
            "SELECT s.id,s.attempts FROM audit_sessions s JOIN tasks t ON t.id=s.task AND t.audit_session_id=s.id
             WHERE t.id=? AND t.user=? AND t.revision=? AND t.state IN ('queued','running')
               AND s.started_at>?",
        )
        .bind(task)
        .bind(user)
        .bind(revision)
        .bind(now() - RETENTION)
        .fetch_optional(db)
        .await?;
        Ok(scope.map(|(session, attempt)| Self {
            db: db.clone(),
            session,
            attempt,
            revision,
        }))
    }

    pub(crate) async fn confirmed_progress(&self) -> Result<ProgressEvidence> {
        let entries: Vec<(String, String)> = sqlx::query_as(
            "SELECT e.name,e.detail FROM audit_sessions s
             JOIN tasks t ON t.id=s.task AND t.audit_session_id=s.id
             JOIN audit_entries e ON e.session=s.id AND e.attempt=s.attempts
             WHERE s.id=?1 AND s.attempts=?2 AND t.revision=?3
               AND t.state='running' AND s.status='running'
               AND e.kind='tool' AND e.status='completed'
               AND e.name IN ('browse','search_web','delegate','send_email','form_submission','send_task_file')
               AND e.id>(SELECT MAX(start.id) FROM audit_entries start
                   WHERE start.session=s.id AND start.attempt=s.attempts AND start.kind='session'
                     AND start.name='agent' AND start.status='running'
                     AND CASE WHEN json_valid(start.detail) THEN json_extract(start.detail,'$.revision') END=?3)
             ORDER BY e.id DESC LIMIT 32",
        )
        .bind(self.session)
        .bind(self.attempt)
        .bind(self.revision)
        .fetch_all(&self.db)
        .await?;
        let mut progress = ProgressEvidence::default();
        for (name, raw) in entries {
            if serde_json::from_str(&raw)
                .ok()
                .and_then(|detail| crate::runtime::confirmed_step(&name, &detail))
                .is_none()
            {
                continue;
            }
            progress.completed_steps += 1;
            progress.searches_with_results += usize::from(name == "search_web");
            progress.pages_read += usize::from(name == "browse");
        }
        Ok(progress)
    }

    pub(crate) async fn start(db: &SqlitePool, task: &Task) -> Result<Option<Self>> {
        let scope: Option<(i64, i64)> = sqlx::query_as(
            "UPDATE audit_sessions SET status='running',finished_at=NULL,attempts=attempts+1,
                first_attempt_at_ms=coalesce(first_attempt_at_ms,CAST(unixepoch('subsec')*1000 AS INTEGER))
             WHERE started_at>? AND id=(SELECT audit_session_id FROM tasks
                 WHERE id=? AND user=? AND revision=? AND state='running')
             RETURNING id,attempts",
        )
        .bind(now() - RETENTION)
        .bind(&task.id)
        .bind(&task.user)
        .bind(task.revision)
        .fetch_optional(db)
        .await?;
        Ok(scope.map(|(session, attempt)| Self {
            db: db.clone(),
            session,
            attempt,
            revision: task.revision,
        }))
    }

    pub(crate) async fn finish(&self, task: &Task, failed: bool) -> Result<()> {
        let state: Option<(String, i64, String)> = sqlx::query_as(
            "SELECT t.state,t.revision,s.status FROM audit_sessions s JOIN tasks t ON t.id=s.task WHERE s.id=?",
        )
            .bind(self.session)
            .fetch_optional(&self.db)
            .await?;
        let Some((state, revision, audited)) = state else {
            return Ok(());
        };
        let status = match audited.as_str() {
            "superseded" | "cancelled" | "completed" | "failed" => audited.as_str(),
            _ if revision != task.revision => "superseded",
            _ => session_status(&state, failed),
        };
        self.push("session", "agent", status, &Value::Null).await?;
        let finished =
            matches!(status, "completed" | "failed" | "superseded" | "cancelled").then(now);
        sqlx::query("UPDATE audit_sessions SET status=?,finished_at=? WHERE id=? AND attempts=? AND status='running'")
            .bind(status)
            .bind(finished)
            .bind(self.session)
            .bind(self.attempt)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    async fn push(
        &self,
        kind: &str,
        name: &str,
        status: &str,
        detail: &Value,
    ) -> Result<Option<i64>> {
        let mut body = detail.to_string();
        if body.len() > 4096 {
            body = r#"{"summary":"Details exceed the preview limit."}"#.into();
        }
        Ok(sqlx::query_scalar(
            "INSERT INTO audit_entries(session,attempt,at,kind,name,status,detail)
             SELECT id,?,?,?,?,?,? FROM audit_sessions WHERE id=? AND started_at>? RETURNING id",
        )
        .bind(self.attempt)
        .bind(now())
        .bind(kind)
        .bind(name)
        .bind(status)
        .bind(body)
        .bind(self.session)
        .bind(now() - RETENTION)
        .fetch_optional(&self.db)
        .await?)
    }
}

pub(crate) async fn scope<T>(trace: Option<Trace>, future: impl Future<Output = T>) -> T {
    CURRENT.scope(trace, future).await
}

pub(crate) fn current_trace() -> Option<Trace> {
    CURRENT.try_with(Clone::clone).ok().flatten()
}

pub(crate) async fn record(kind: &str, name: &str, status: &str, detail: Value) {
    if let Ok(Some(trace)) = CURRENT.try_with(Clone::clone)
        && trace.push(kind, name, status, &detail).await.is_err()
    {
        tracing::warn!("unable to record agent audit entry");
    }
}

pub(crate) async fn model<T>(
    name: &'static str,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    record("model", name, "started", Value::Null).await;
    let start = Instant::now();
    let output = MODEL_SCOPE.scope(name, future).await;
    let mut detail =
        json!({"duration_ms": start.elapsed().as_millis() as u64, "usage_tracking":true});
    if let Err(error) = &output
        && let Some(failure) = provider_failure(error)
    {
        detail["category"] = json!(failure.category());
    }
    record(
        "model",
        name,
        if output.is_ok() {
            "completed"
        } else {
            "failed"
        },
        detail,
    )
    .await;
    output
}

pub(crate) async fn model_selected(provider: &str, id: &str, review: bool) {
    record(
        "model",
        if review {
            "action_review"
        } else {
            "agent_turn"
        },
        "selected",
        json!({"provider":provider,"model":details::message_preview(id)}),
    )
    .await;
}

pub(crate) async fn tool_started(name: &str, args: &Value) {
    record("tool", name, "started", details::arguments(name, args)).await;
}

pub(crate) async fn tool_finished(name: &str, output: &Result<Outcome>) {
    let (status, detail) = match output {
        Ok(Outcome::Continue(value)) => ("completed", details::result(name, value)),
        Ok(Outcome::Pause) if matches!(name, "send_card" | "send_reply") => {
            ("completed", Value::Null)
        }
        Ok(Outcome::Pause) => ("paused", Value::Null),
        Ok(Outcome::Media(_)) => ("prepared", Value::Null),
        Err(error) => ("failed", json!({"category":details::error_category(error)})),
    };
    record("tool", name, status, detail).await;
}

pub(crate) async fn open(
    tx: &mut Transaction<'_, Sqlite>,
    task: &str,
    message: &str,
) -> Result<()> {
    let time = now();
    sqlx::query(
        "UPDATE audit_sessions SET status='superseded',finished_at=?
         WHERE id=(SELECT audit_session_id FROM tasks WHERE id=?)
           AND status IN ('queued','setup','running','waiting','uncertain','interrupted')",
    )
    .bind(time)
    .bind(task)
    .execute(&mut **tx)
    .await?;
    let session: i64 = sqlx::query_scalar(
        "INSERT INTO audit_sessions(task,message,started_at,status,received_at_ms)
         SELECT id,?,?,CASE WHEN state IN ('setup','uncertain') THEN state ELSE 'queued' END,
                CAST(unixepoch('subsec')*1000 AS INTEGER)
         FROM tasks WHERE id=? RETURNING id",
    )
    .bind(details::message_preview(message))
    .bind(time)
    .bind(task)
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query("UPDATE tasks SET audit_session_id=? WHERE id=?")
        .bind(session)
        .bind(task)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub(crate) fn safe_history_text(value: &str) -> Option<&str> {
    (value.is_empty() || details::message_preview(value) == value).then_some(value)
}

pub(crate) async fn cancel(tx: &mut Transaction<'_, Sqlite>, task: &str, user: &str) -> Result<()> {
    sqlx::query(
        "UPDATE audit_sessions SET status='cancelled',finished_at=?
         WHERE status IN ('queued','setup','running','waiting','uncertain','interrupted')
           AND id=(SELECT audit_session_id FROM tasks WHERE id=? AND user=? AND state='cancelled')",
    )
    .bind(now())
    .bind(task)
    .bind(user)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn task_completed(tx: &mut Transaction<'_, Sqlite>, task: &Task) -> Result<()> {
    let at = now();
    sqlx::query(
        "INSERT INTO audit_entries(session,attempt,at,kind,name,status,detail)
         SELECT s.id,s.attempts,?,'task','marked_complete','completed','null'
         FROM tasks t JOIN audit_sessions s ON s.id=t.audit_session_id
         WHERE t.id=? AND s.started_at>?",
    )
    .bind(at)
    .bind(&task.id)
    .bind(at - RETENTION)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE audit_sessions SET status='completed',finished_at=?
         WHERE status IN ('queued','setup','running','waiting','interrupted')
           AND id=(SELECT audit_session_id FROM tasks WHERE id=?)",
    )
    .bind(at)
    .bind(&task.id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn session_status(state: &str, failed: bool) -> &'static str {
    match state {
        "cancelled" | "revoked" => "cancelled",
        "waiting" => "waiting",
        "uncertain" => "uncertain",
        "blocked" => "failed",
        "queued" => "queued",
        _ if failed => "failed",
        _ => "completed",
    }
}

pub(crate) async fn mark_task(
    tx: &mut Transaction<'_, Sqlite>,
    task: &Task,
    state: &str,
) -> Result<()> {
    let status = session_status(state, false);
    sqlx::query(
        "UPDATE audit_sessions SET status=?,finished_at=?
         WHERE id=(SELECT audit_session_id FROM tasks WHERE id=? AND revision=?)",
    )
    .bind(status)
    .bind(matches!(status, "completed" | "cancelled" | "failed").then(now))
    .bind(&task.id)
    .bind(task.revision)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn request_changed(
    tx: &mut Transaction<'_, Sqlite>,
    task: &str,
    revision: i64,
    name: &str,
    status: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO audit_entries(session,attempt,at,kind,name,status,detail)
         SELECT s.id,s.attempts,?,'request',?,?,'null'
         FROM tasks t JOIN audit_sessions s ON t.audit_session_id=s.id
         WHERE t.id=? AND t.revision=? AND s.started_at>?",
    )
    .bind(now())
    .bind(name)
    .bind(status)
    .bind(task)
    .bind(revision)
    .bind(now() - RETENTION)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn recover(tx: &mut Transaction<'_, Sqlite>) -> Result<()> {
    sqlx::query(
        "INSERT INTO audit_entries(session,attempt,at,kind,name,status,detail)
         SELECT id,attempts,?,'session','agent','interrupted','null' FROM audit_sessions
         WHERE status='running' AND started_at>?",
    )
    .bind(now())
    .bind(now() - RETENTION)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE audit_sessions SET status=CASE WHEN EXISTS(
            SELECT 1 FROM tasks WHERE audit_session_id=audit_sessions.id AND state='uncertain')
            THEN 'uncertain' ELSE 'interrupted' END
         WHERE status='running'",
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn prune(tx: &mut Transaction<'_, Sqlite>) -> Result<()> {
    sqlx::query("DELETE FROM audit_sessions WHERE started_at<=?")
        .bind(now() - RETENTION)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

// All audit reads require both ownership of the message and current access to the agent.
const VISIBLE_SESSIONS: &str =
    "FROM audit_sessions s JOIN tasks t ON s.task=t.id LEFT JOIN agents a ON t.agent=a.id
     WHERE t.user=? AND s.started_at>? AND t.state<>'revoked'
       AND (t.agent IS NULL OR a.owner=t.user OR EXISTS(
           SELECT 1 FROM grants g WHERE g.agent=t.agent AND g.member=t.user))";
const SESSION_FIELDS: &str =
    "SELECT s.id,s.task,a.name AS agent,s.message,s.started_at,s.finished_at,
       s.status,(SELECT count(*) FROM audit_entries e WHERE e.session=s.id) AS entry_count";

#[derive(FromRow, Serialize)]
struct Session {
    id: i64,
    task: String,
    agent: Option<String>,
    message: String,
    started_at: i64,
    finished_at: Option<i64>,
    status: String,
    entry_count: i64,
}

pub(crate) async fn sessions(db: &SqlitePool, user: &str, before: Option<i64>) -> Result<Value> {
    let cutoff = now() - RETENTION;
    let mut tx = db.begin().await?;
    let sql = format!(
        "{SESSION_FIELDS} {VISIBLE_SESSIONS} AND (? IS NULL OR s.id<?) ORDER BY s.id DESC LIMIT ?"
    );
    let mut rows: Vec<Session> = sqlx::query_as(&sql)
        .bind(user)
        .bind(cutoff)
        .bind(before)
        .bind(before)
        .bind(SESSION_PAGE as i64 + 1)
        .fetch_all(&mut *tx)
        .await?;
    let usage = usage::summary(&mut tx, user, None, cutoff).await?;
    let delivery = delivery::summary(&mut tx, user, cutoff).await?;
    tx.commit().await?;
    let next = (rows.len() > SESSION_PAGE).then(|| rows[SESSION_PAGE - 1].id);
    rows.truncate(SESSION_PAGE);
    Ok(json!({"sessions":rows,"next_before":next,"usage":usage,"delivery":delivery}))
}

pub(crate) async fn account_usage(
    connection: &mut sqlx::SqliteConnection,
    user: &str,
) -> Result<Value> {
    let cutoff = now() - RETENTION;
    let usage = usage::summary(connection, user, None, cutoff).await?;
    let delivery = delivery::summary(connection, user, cutoff).await?;
    Ok(json!({"usage":usage["total"],"delivery":delivery}))
}

#[derive(FromRow)]
struct Entry {
    id: i64,
    attempt: i64,
    at: i64,
    kind: String,
    name: String,
    status: String,
    detail: String,
}

pub(crate) async fn session(db: &SqlitePool, user: &str, id: i64, after: i64) -> Result<Value> {
    let (session, mut rows, usage, delivery) =
        session_snapshot(db, user, id, after, ENTRY_PAGE as i64 + 1).await?;
    let next = (rows.len() > ENTRY_PAGE).then(|| rows[ENTRY_PAGE - 1].id);
    rows.truncate(ENTRY_PAGE);
    Ok(
        json!({"session":session,"entries":entry_details(rows),"next_after":next,"usage":usage,"delivery":delivery}),
    )
}

pub(crate) async fn export(db: &SqlitePool, user: &str, id: i64) -> Result<Value> {
    // SQLite treats a negative LIMIT as unbounded.
    let (session, rows, usage, delivery) = session_snapshot(db, user, id, 0, -1).await?;
    Ok(
        json!({"version":1,"session":session,"entries":entry_details(rows),"usage":usage,"delivery":delivery}),
    )
}

async fn session_snapshot(
    db: &SqlitePool,
    user: &str,
    id: i64,
    after: i64,
    limit: i64,
) -> Result<(Session, Vec<Entry>, Value, Value)> {
    let cutoff = now() - RETENTION;
    let mut tx = db.begin().await?;
    let sql = format!("{SESSION_FIELDS} {VISIBLE_SESSIONS} AND s.id=?");
    let session: Session = sqlx::query_as(&sql)
        .bind(user)
        .bind(cutoff)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .context("audit session unavailable")?;
    let rows = sqlx::query_as(
        "SELECT id,attempt,at,kind,name,status,detail FROM audit_entries
         WHERE session=? AND id>? ORDER BY id LIMIT ?",
    )
    .bind(id)
    .bind(after)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    let usage = usage::summary(&mut tx, user, Some(id), cutoff).await?;
    let delivery = delivery::session(&mut tx, id).await?;
    tx.commit().await?;
    Ok((session, rows, usage, delivery))
}

fn entry_details(rows: Vec<Entry>) -> Vec<Value> {
    rows.into_iter().map(|row| json!({
        "id":row.id,"attempt":row.attempt,"at":row.at,"kind":row.kind,"name":row.name,"status":row.status,
        "detail":serde_json::from_str::<Value>(&row.detail).unwrap_or(Value::Null)
    })).collect()
}
