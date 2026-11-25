use super::{Outbound, Task, has_access};
use crate::security::{Vault, hash};
use anyhow::{Context, Result, ensure};
use sqlx::{Sqlite, SqlitePool, Transaction};

pub const BUTTON: &str = "telegram-output";

pub(crate) struct Output {
    pub kind: String,
    pub name: String,
    pub bytes: Vec<u8>,
    pub synthesized: bool,
}

pub(crate) async fn queue_document(
    db: &SqlitePool,
    vault: &Vault,
    task: &Task,
    name: &str,
) -> Result<bool> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let body: String = sqlx::query_scalar(
        "SELECT f.body FROM files f JOIN tasks t ON t.id=f.task
         WHERE f.task=? AND f.name=? AND t.user=? AND t.revision=? AND t.state='running'
            AND t.agent IS NULL AND t.chief_chat=1 AND t.shared_memory_pair IS NULL
            AND EXISTS(SELECT 1 FROM users u WHERE u.id=t.user AND u.suspended_at IS NULL)",
    )
    .bind(&task.id)
    .bind(name)
    .bind(&task.user)
    .bind(task.revision)
    .fetch_optional(&mut *tx)
    .await?
    .context("task file is unavailable")?;
    ensure!(
        !body.is_empty() && body.len() <= 32_768,
        "invalid task file"
    );
    let artifact = super::artifacts::remember_document(&mut tx, vault, task, name, &body).await?;
    let queued = queue(
        &mut tx,
        vault,
        &task.user,
        &task.id,
        task.revision,
        "document",
        name,
        &body,
        None,
        artifact.as_deref(),
    )
    .await?;
    tx.commit().await?;
    Ok(queued)
}

pub(crate) async fn queue_voice(
    db: &SqlitePool,
    vault: &Vault,
    user: &str,
    outbox: i64,
    message: i64,
) -> Result<bool> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let item: Option<(String, i64, String)> = sqlx::query_as(
        "SELECT t.id,t.revision,e.body FROM outbox o JOIN events e ON e.id=o.assistant_event_id
         JOIN tasks t ON t.id=o.task AND t.id=e.task
         WHERE o.id=? AND o.telegram_message_id=? AND o.user=? AND o.state='sent'
            AND t.user=o.user AND t.agent IS NULL AND t.chief_chat=1
            AND t.shared_memory_pair IS NULL
            AND EXISTS(SELECT 1 FROM users u WHERE u.id=t.user AND u.suspended_at IS NULL)
            AND t.state NOT IN ('cancelled','revoked')",
    )
    .bind(outbox)
    .bind(message)
    .bind(user)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((task, revision, body)) = item else {
        return Ok(false);
    };
    let body = crate::security::clip(body.trim(), 4_000);
    if body.is_empty() {
        return Ok(false);
    }
    let queued = queue(
        &mut tx,
        vault,
        user,
        &task,
        revision,
        "voice",
        "reply.mp3",
        body,
        Some(message),
        None,
    )
    .await?;
    tx.commit().await?;
    Ok(queued)
}

async fn queue(
    tx: &mut Transaction<'_, Sqlite>,
    vault: &Vault,
    user: &str,
    task: &str,
    revision: i64,
    kind: &str,
    name: &str,
    content: &str,
    reply: Option<i64>,
    artifact: Option<&str>,
) -> Result<bool> {
    let fingerprint = hash(content);
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM telegram_outputs f JOIN outbox o ON o.id=f.outbox
         WHERE o.task=? AND o.user=? AND f.revision=? AND f.kind=? AND f.name=? AND f.fingerprint=?
            AND (o.state IN ('queued','sending','uncertain') OR (?='document' AND o.state='sent')))",
    )
    .bind(task)
    .bind(user)
    .bind(revision)
    .bind(kind)
    .bind(name)
    .bind(&fingerprint)
    .bind(kind)
    .fetch_one(&mut **tx)
    .await?;
    if exists {
        return Ok(false);
    }
    let outbox = sqlx::query(
        "INSERT INTO outbox(user,task,body,button,reply_to_message_id) VALUES(?,?,'',?,?)",
    )
    .bind(user)
    .bind(task)
    .bind(BUTTON)
    .bind(reply)
    .execute(&mut **tx)
    .await?
    .last_insert_rowid();
    let ciphertext = vault.seal(&binding(user, outbox), content.as_bytes())?;
    sqlx::query("INSERT INTO telegram_outputs(outbox,revision,kind,name,fingerprint,ciphertext,artifact) VALUES(?,?,?,?,?,?,?)")
        .bind(outbox)
        .bind(revision)
        .bind(kind)
        .bind(name)
        .bind(fingerprint)
        .bind(ciphertext)
        .bind(artifact)
        .execute(&mut **tx)
        .await?;
    Ok(true)
}

pub(crate) async fn load(
    db: &SqlitePool,
    vault: &Vault,
    item: &Outbound,
) -> Result<Option<Output>> {
    if !has_access(db, &item.user, item.agent.as_deref()).await? {
        return Ok(None);
    }
    let output: Option<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT f.kind,f.name,f.ciphertext,f.audio FROM telegram_outputs f JOIN outbox o ON o.id=f.outbox
         JOIN tasks t ON t.id=o.task AND t.revision=f.revision AND t.user=o.user
         WHERE o.id=? AND o.user=? AND o.state='sending' AND t.agent IS NULL
           AND t.shared_memory_pair IS NULL
           AND EXISTS(SELECT 1 FROM users u WHERE u.id=t.user AND u.suspended_at IS NULL)
           AND t.state NOT IN ('cancelled','revoked') AND f.ciphertext IS NOT NULL",
    )
    .bind(item.id)
    .bind(&item.user)
    .fetch_optional(db)
    .await?;
    output
        .map(|(kind, name, ciphertext, audio)| {
            let bytes = match &audio {
                Some(audio) => vault.open(&audio_binding(item), audio)?,
                None => vault.open(&binding(&item.user, item.id), &ciphertext)?,
            };
            Ok(Output {
                kind,
                name,
                bytes,
                synthesized: audio.is_some(),
            })
        })
        .transpose()
}

pub(crate) async fn save_audio(
    db: &SqlitePool,
    vault: &Vault,
    item: &Outbound,
    bytes: &[u8],
) -> Result<()> {
    sqlx::query(
        "UPDATE telegram_outputs SET audio=? WHERE outbox=? AND kind='voice' AND ciphertext IS NOT NULL",
    )
    .bind(vault.seal(&audio_binding(item), bytes)?)
    .bind(item.id)
    .execute(db)
    .await?;
    Ok(())
}

fn audio_binding(item: &Outbound) -> String {
    format!("telegram-output-audio:{}:{}", item.user, item.id)
}

fn binding(user: &str, outbox: i64) -> String {
    format!("telegram-output:{user}:{outbox}")
}
