use super::{Task, requests};
use crate::security::{Vault, now};
use anyhow::{Result, ensure};
use serde::Serialize;
use serde_json::Value;
use sqlx::{Executor, FromRow, Sqlite, SqlitePool};
use std::collections::BTreeSet;

pub(crate) const MAX_MODEL_PASSES: i64 = 112;
pub(crate) const MAX_PLAN_STEPS: usize = 8;
pub(crate) const MAX_TITLE_BYTES: usize = 120;
pub(crate) const MAX_CHECKPOINT_BYTES: usize = 360;

const PRIVATE_RUNNING: &str = "t.id=?1 AND t.user=?2 AND t.revision=?3 AND t.state='running'
    AND t.chief_chat=1 AND t.agent IS NULL AND t.shared_memory_pair IS NULL AND t.schedule IS NULL
    AND EXISTS(SELECT 1 FROM users u WHERE u.id=t.user AND u.suspended_at IS NULL)";

#[derive(FromRow, Serialize)]
pub(crate) struct Step {
    pub position: i64,
    pub title: String,
    pub checkpoint: Option<String>,
    pub completed_at: Option<i64>,
}

#[derive(Default)]
pub(crate) struct ReadCache {
    pub keys: BTreeSet<String>,
    pub evidence: Vec<Value>,
}

pub(crate) async fn can_plan<'a>(
    db: impl Executor<'a, Database = Sqlite>,
    task: &Task,
) -> Result<bool> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT EXISTS(SELECT 1 FROM tasks t WHERE {PRIVATE_RUNNING})"
    ))
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .fetch_one(db)
    .await?)
}

pub(crate) async fn reserve_model(db: &SqlitePool, task: &Task) -> Result<bool> {
    Ok(sqlx::query(
        "INSERT INTO task_model_passes(task,revision,passes)
         SELECT t.id,t.revision,1 FROM tasks t WHERE t.id=? AND t.user=? AND t.revision=?
         AND t.state='running' AND EXISTS(SELECT 1 FROM users u WHERE u.id=t.user AND u.suspended_at IS NULL)
         ON CONFLICT(task,revision) DO UPDATE SET passes=passes+1 WHERE passes<?",
    )
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .bind(MAX_MODEL_PASSES)
    .execute(db)
    .await?
    .rows_affected() == 1)
}

pub(crate) async fn plan<'a>(
    db: impl Executor<'a, Database = Sqlite>,
    task: &str,
    revision: i64,
    archived: bool,
) -> Result<Vec<Step>> {
    Ok(sqlx::query_as(
        "SELECT position,title,checkpoint,completed_at FROM task_plan_steps
         WHERE task=? AND revision=(SELECT max(revision) FROM task_plan_steps
                 WHERE task=? AND (revision=? OR (? AND revision=?-1))) ORDER BY position",
    )
    .bind(task)
    .bind(task)
    .bind(revision)
    .bind(archived)
    .bind(revision)
    .fetch_all(db)
    .await?)
}

pub(crate) async fn save_plan(db: &SqlitePool, task: &Task, titles: &[&str]) -> Result<Vec<Step>> {
    let unique: BTreeSet<_> = titles.iter().map(|title| title.to_lowercase()).collect();
    ensure!(
        (2..=MAX_PLAN_STEPS).contains(&titles.len())
            && unique.len() == titles.len()
            && titles
                .iter()
                .all(|title| valid_line(title, MAX_TITLE_BYTES)),
        "provide two to eight distinct, short plan steps"
    );
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    ensure!(
        can_plan(&mut *tx, task).await?,
        "task changed before planning"
    );
    let mut steps = plan(&mut *tx, &task.id, task.revision, false).await?;
    if steps.is_empty() {
        for (position, title) in titles.iter().enumerate() {
            sqlx::query(
                "INSERT INTO task_plan_steps(task,revision,position,title) VALUES(?,?,?,?)",
            )
            .bind(&task.id)
            .bind(task.revision)
            .bind(position as i64)
            .bind(title)
            .execute(&mut *tx)
            .await?;
        }
        steps = plan(&mut *tx, &task.id, task.revision, false).await?;
    }
    tx.commit().await?;
    Ok(steps)
}

pub(crate) async fn record_step(
    db: &SqlitePool,
    task: &Task,
    position: i64,
    checkpoint: &str,
) -> Result<Vec<Step>> {
    ensure!(
        valid_line(checkpoint, MAX_CHECKPOINT_BYTES),
        "provide a short checkpoint"
    );
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    ensure!(
        can_plan(&mut *tx, task).await?,
        "task changed before recording progress"
    );
    let updated = sqlx::query(
        "UPDATE task_plan_steps SET checkpoint=?3,completed_at=?4
         WHERE task=?1 AND revision=?2 AND position=?5 AND completed_at IS NULL
         AND position=(SELECT min(position) FROM task_plan_steps
             WHERE task=?1 AND revision=?2 AND completed_at IS NULL)",
    )
    .bind(&task.id)
    .bind(task.revision)
    .bind(checkpoint)
    .bind(now())
    .bind(position)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let steps = plan(&mut *tx, &task.id, task.revision, false).await?;
    ensure!(
        updated == 1
            || steps.iter().any(|step| {
                step.position == position && step.checkpoint.as_deref() == Some(checkpoint)
            }),
        "record the current step only"
    );
    tx.commit().await?;
    Ok(steps)
}

fn valid_line(text: &str, limit: usize) -> bool {
    !text.trim().is_empty() && text.len() <= limit && !text.chars().any(char::is_control)
}

pub(crate) async fn finish_read(
    db: &SqlitePool,
    vault: &Vault,
    task: &Task,
    action: &str,
    full_result: &str,
    verified: Option<(&str, &str)>,
) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let current: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=? AND user=? AND revision=? AND state='running')",
    )
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .fetch_one(&mut *tx)
    .await?;
    if !current {
        requests::finish_pending_in(&mut tx, task, action).await?;
        tx.commit().await?;
        return Ok(());
    }
    requests::finish_action_in(&mut tx, task, action, full_result, false).await?;
    if let Some((key, observation)) = verified {
        let ciphertext = vault.seal(&binding(task, key), observation.as_bytes())?;
        sqlx::query(&format!(
            "INSERT INTO task_verified_reads(task,revision,lookup_key,recorded_at,ciphertext)
             SELECT t.id,t.revision,?4,?5,?6 FROM tasks t WHERE {PRIVATE_RUNNING}
             ON CONFLICT(task,revision,lookup_key) DO UPDATE
                 SET recorded_at=excluded.recorded_at,ciphertext=excluded.ciphertext"
        ))
        .bind(&task.id)
        .bind(&task.user)
        .bind(task.revision)
        .bind(key)
        .bind(now())
        .bind(ciphertext)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn read_cache(db: &SqlitePool, vault: &Vault, task: &Task) -> Result<ReadCache> {
    let rows: Vec<(String, i64, Option<String>)> = sqlx::query_as(&format!(
        "SELECT v.lookup_key,v.recorded_at,
             CASE WHEN row_number() OVER (ORDER BY v.id DESC)<=3 THEN v.ciphertext END
         FROM task_verified_reads v JOIN tasks t ON t.id=v.task AND t.revision=v.revision
         WHERE {PRIVATE_RUNNING} ORDER BY v.id DESC"
    ))
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .fetch_all(db)
    .await?;
    let mut cache = ReadCache::default();
    for (key, recorded_at, ciphertext) in rows {
        if let Some(ciphertext) = ciphertext {
            let Some(observation) = open_evidence(vault, task, &key, recorded_at, &ciphertext)
            else {
                continue;
            };
            cache.evidence.push(observation);
        }
        cache.keys.insert(key);
    }
    Ok(cache)
}

pub(crate) async fn cached_read(
    db: &SqlitePool,
    vault: &Vault,
    task: &Task,
    key: &str,
) -> Result<Option<Value>> {
    let saved: Option<(i64, String)> = sqlx::query_as(&format!(
        "SELECT v.recorded_at,v.ciphertext FROM task_verified_reads v
         JOIN tasks t ON t.id=v.task AND t.revision=v.revision
         WHERE {PRIVATE_RUNNING} AND v.lookup_key=?4"
    ))
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .bind(key)
    .fetch_optional(db)
    .await?;
    Ok(saved.and_then(|(at, ciphertext)| open_evidence(vault, task, key, at, &ciphertext)))
}

fn open_evidence(vault: &Vault, task: &Task, key: &str, at: i64, blob: &str) -> Option<Value> {
    let bytes = vault.open(&binding(task, key), blob).ok()?;
    let mut evidence = serde_json::from_slice::<Value>(&bytes).ok()?;
    evidence
        .as_object_mut()?
        .insert("read_at".into(), at.into());
    Some(evidence)
}

fn binding(task: &Task, key: &str) -> String {
    format!(
        "task-read:{}:{}:{}:{key}",
        task.user, task.id, task.revision
    )
}
