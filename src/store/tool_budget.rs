use super::Task;
use crate::security::hash;
use anyhow::{Result, ensure};
use serde::Serialize;
use serde_json::Value;
use sqlx::{FromRow, SqlitePool};

pub const INTERVAL: i64 = 10;
pub const LIMIT: i64 = 100;

// The ledger belongs to the current task revision, even when a helper invokes the tool.
const CURRENT: &str = "t.id=?1 AND t.user=?2 AND t.revision=?3 AND t.agent IS ?4 AND t.state='running'
    AND (t.agent IS NULL OR EXISTS (SELECT 1 FROM agents a WHERE a.id=t.agent AND
        (a.owner=t.user OR EXISTS (SELECT 1 FROM grants g WHERE g.agent=a.id AND g.member=t.user))))";

#[derive(Debug, FromRow, Serialize)]
pub struct ToolAttempt {
    pub position: i64,
    pub actor: String,
    pub tool: String,
    pub args_fingerprint: String,
}

#[derive(Debug)]
pub struct CheckBatch {
    pub position: i64,
    pub attempts: Vec<ToolAttempt>,
}

#[derive(Debug)]
pub struct Progress {
    pub calls: i64,
    pub stalled: bool,
    pub pending: Option<CheckBatch>,
}

pub async fn record(
    db: &SqlitePool,
    task: &Task,
    actor: &str,
    tool: &str,
    args: &Value,
) -> Result<bool> {
    ensure!(matches!(actor, "chief" | "helper"), "invalid tool actor");
    ensure!(
        !tool.is_empty()
            && tool.len() <= 128
            && tool
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte)),
        "invalid tool name"
    );
    let mut normalized = args.clone();
    normalized.sort_all_objects();
    let fingerprint = hash(&format!(
        "{}:{}:{tool}:{normalized}",
        task.id, task.revision
    ));
    let statement = format!(
        "WITH progress AS (
            SELECT COALESCE(MAX(position),0) AS calls FROM task_tool_attempts WHERE task=?1 AND revision=?3
         )
         INSERT INTO task_tool_attempts(task,revision,position,actor,tool,args_fingerprint)
         SELECT t.id,t.revision,p.calls+1,?5,?6,?7 FROM tasks t CROSS JOIN progress p
         LEFT JOIN task_tool_checks c ON c.task=t.id AND c.revision=t.revision AND c.position=p.calls
         WHERE {CURRENT} AND p.calls<?8 AND (p.calls=0 OR p.calls%?9<>0 OR c.stalled=0)",
    );
    let attempts = sqlx::query(&statement)
        .bind(&task.id)
        .bind(&task.user)
        .bind(task.revision)
        .bind(&task.agent)
        .bind(actor)
        .bind(tool)
        .bind(fingerprint)
        .bind(LIMIT)
        .bind(INTERVAL)
        .execute(db)
        .await?;
    Ok(attempts.rows_affected() == 1)
}

pub async fn progress(db: &SqlitePool, task: &Task) -> Result<Option<Progress>> {
    let mut tx = db.begin().await?;
    let statement = format!(
        "WITH progress AS (
            SELECT COALESCE(MAX(position),0) AS calls FROM task_tool_attempts WHERE task=?1 AND revision=?3
         )
         SELECT p.calls,c.stalled FROM tasks t CROSS JOIN progress p
         LEFT JOIN task_tool_checks c ON c.task=t.id AND c.revision=t.revision AND c.position=p.calls
         WHERE {CURRENT}",
    );
    let row: Option<(i64, Option<bool>)> = sqlx::query_as(&statement)
        .bind(&task.id)
        .bind(&task.user)
        .bind(task.revision)
        .bind(&task.agent)
        .fetch_optional(&mut *tx)
        .await?;
    let Some((calls, checked)) = row else {
        return Ok(None);
    };
    let pending = if checked.is_none() && calls > 0 && calls % INTERVAL == 0 {
        let attempts = sqlx::query_as(
            "SELECT position,actor,tool,args_fingerprint FROM task_tool_attempts
             WHERE task=? AND revision=? AND position>? AND position<=? ORDER BY position",
        )
        .bind(&task.id)
        .bind(task.revision)
        .bind(calls - INTERVAL)
        .bind(calls)
        .fetch_all(&mut *tx)
        .await?;
        Some(CheckBatch {
            position: calls,
            attempts,
        })
    } else {
        None
    };
    tx.commit().await?;
    Ok(Some(Progress {
        calls,
        stalled: checked == Some(true),
        pending,
    }))
}

pub async fn complete_check(
    db: &SqlitePool,
    task: &Task,
    position: i64,
    stalled: bool,
) -> Result<bool> {
    ensure!(
        (INTERVAL..=LIMIT).contains(&position) && position % INTERVAL == 0,
        "invalid tool checkpoint"
    );
    let statement = format!(
        "INSERT INTO task_tool_checks(task,revision,position,stalled)
         SELECT t.id,t.revision,?5,?6 FROM tasks t WHERE {CURRENT}
         AND (SELECT MAX(position) FROM task_tool_attempts WHERE task=t.id AND revision=t.revision)=?5
         ON CONFLICT(task,revision,position) DO NOTHING",
    );
    let completed = sqlx::query(&statement)
        .bind(&task.id)
        .bind(&task.user)
        .bind(task.revision)
        .bind(&task.agent)
        .bind(position)
        .bind(stalled)
        .execute(db)
        .await?;
    Ok(completed.rows_affected() == 1)
}

#[cfg(test)]
mod tests;
