use super::{Task, attachments, task_views};
use crate::security::{Vault, hash, id, now};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};

const RETENTION: i64 = 7 * 24 * 60 * 60;
const MAX_ARTIFACTS_PER_USER: i64 = 64;

#[derive(FromRow, Serialize)]
pub(crate) struct Item {
    id: String,
    kind: String,
    name: String,
    version: Option<i64>,
    bytes: Option<i64>,
    created_at: Option<i64>,
    expires_at: Option<i64>,
    delivery: Option<String>,
}

pub(crate) struct Download {
    pub(crate) name: String,
    pub(crate) bytes: Vec<u8>,
}

pub(crate) async fn remember_document(
    tx: &mut Transaction<'_, Sqlite>,
    vault: &Vault,
    task: &Task,
    name: &str,
    body: &str,
) -> Result<Option<String>> {
    let permitted: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM tasks t JOIN users u ON u.id=t.user
         WHERE t.id=? AND t.user=? AND t.revision=? AND t.state='running' AND t.chief_chat=1
           AND t.agent IS NULL AND t.shared_memory_pair IS NULL AND u.suspended_at IS NULL)",
    )
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .fetch_one(&mut **tx)
    .await?;
    if !permitted {
        return Ok(None);
    }
    let fingerprint = hash(body);
    let time = now();
    let latest: Option<(String, i64, String, i64)> = sqlx::query_as(
        "SELECT id, version, fingerprint, expires_at FROM task_artifacts
         WHERE task=? AND user=? AND name=? ORDER BY version DESC LIMIT 1",
    )
    .bind(&task.id)
    .bind(&task.user)
    .bind(name)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((saved, _, previous, expires)) = &latest
        && *expires > time
        && previous == &fingerprint
    {
        return Ok(Some(saved.clone()));
    }
    let artifact = id();
    let version = latest.map_or(1, |(_, version, ..)| version + 1);
    let ciphertext = vault.seal(&binding(&task.user, &task.id, &artifact), body.as_bytes())?;
    sqlx::query(
        "INSERT INTO task_artifacts(id,task,user,name,version,fingerprint,bytes,ciphertext,created_at,expires_at)
         VALUES(?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&artifact)
    .bind(&task.id)
    .bind(&task.user)
    .bind(name)
    .bind(version)
    .bind(fingerprint)
    .bind(body.len() as i64)
    .bind(ciphertext)
    .bind(time)
    .bind(time + RETENTION)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "DELETE FROM task_artifacts WHERE id IN (
            SELECT id FROM task_artifacts WHERE user=? ORDER BY created_at DESC,rowid DESC LIMIT -1 OFFSET ?)",
    )
    .bind(&task.user)
    .bind(MAX_ARTIFACTS_PER_USER)
    .execute(&mut **tx)
    .await?;
    Ok(Some(artifact))
}

pub(crate) async fn list(db: &SqlitePool, user: &str, task: &str) -> Result<Vec<Item>> {
    let mut tx = db.begin().await?;
    if !available(&mut tx, user, task).await? {
        return Ok(Vec::new());
    }
    let mut items = sqlx::query_as(
        "SELECT id,'file' AS kind,name,version,bytes,created_at,expires_at,
            (SELECT o.state FROM telegram_outputs f JOIN outbox o ON o.id=f.outbox
             WHERE f.artifact=a.id AND o.user=a.user AND o.task=a.task ORDER BY o.id DESC LIMIT 1) AS delivery
         FROM task_artifacts a WHERE task=? AND user=? AND expires_at>? ORDER BY created_at DESC,rowid DESC",
    )
    .bind(task)
    .bind(user)
    .bind(now())
    .fetch_all(&mut *tx)
    .await?;
    let uploads: Vec<Item> = sqlx::query_as(
        "SELECT CAST(e.id AS TEXT) AS id,'upload' AS kind,a.name,NULL AS version,NULL AS bytes,
                NULL AS created_at,NULL AS expires_at,NULL AS delivery
         FROM telegram_attachments a JOIN events e ON e.id=a.event
         WHERE e.task=? AND e.role='user' AND e.telegram_message_id>0 ORDER BY e.id DESC LIMIT 40",
    )
    .bind(task)
    .fetch_all(&mut *tx)
    .await?;
    items.extend(uploads);
    tx.commit().await?;
    Ok(items)
}

pub(crate) async fn download(
    db: &SqlitePool,
    vault: &Vault,
    user: &str,
    task: &str,
    artifact: &str,
    kind: &str,
) -> Result<Download> {
    let mut tx = db.begin().await?;
    ensure!(
        available(&mut tx, user, task).await?,
        "task files unavailable"
    );
    let (name, ciphertext, key, max_size) = if kind == "upload" {
        let event: i64 = artifact.parse()?;
        let (name, ciphertext, message): (String, String, i64) = sqlx::query_as(
            "SELECT a.name,a.ciphertext,e.telegram_message_id
             FROM telegram_attachments a JOIN events e ON e.id=a.event
             WHERE a.event=? AND e.task=? AND e.role='user' AND e.telegram_message_id>0",
        )
        .bind(event)
        .bind(task)
        .fetch_optional(&mut *tx)
        .await?
        .context("upload unavailable")?;
        (
            name,
            ciphertext,
            attachments::binding(user, message),
            attachments::MAX_ATTACHMENT_BYTES,
        )
    } else {
        ensure!(kind == "file", "invalid task file kind");
        let (name, ciphertext): (String, String) = sqlx::query_as(
            "SELECT name,ciphertext FROM task_artifacts WHERE id=? AND task=? AND user=? AND expires_at>?",
        )
        .bind(artifact)
        .bind(task)
        .bind(user)
        .bind(now())
        .fetch_optional(&mut *tx)
        .await?
        .context("task file unavailable")?;
        (name, ciphertext, binding(user, task, artifact), 32_768)
    };
    let bytes = vault.open(&key, &ciphertext)?;
    ensure!(bytes.len() <= max_size, "task file exceeds its limit");
    tx.commit().await?;
    Ok(Download { name, bytes })
}

pub(super) async fn prune(tx: &mut Transaction<'_, Sqlite>) -> Result<()> {
    sqlx::query("DELETE FROM task_artifacts WHERE expires_at<=?")
        .bind(now())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn available(tx: &mut Transaction<'_, Sqlite>, user: &str, task: &str) -> Result<bool> {
    Ok(task_views::visible(&mut **tx, user, task)
        .await?
        .is_some_and(|task| task.agent.is_none() && task.shared_memory_pair.is_none()))
}

fn binding(user: &str, task: &str, artifact: &str) -> String {
    format!("task-artifact:{user}:{task}:{artifact}")
}
