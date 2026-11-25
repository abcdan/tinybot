use super::{Outbound, Task, has_access};
use crate::security::{Vault, now};
use anyhow::{Context, Result, ensure};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Sqlite, SqlitePool, Transaction};

pub const BUTTON: &str = "telegram-output";
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_VIDEO_BYTES: usize = 20 * 1024 * 1024;
const MEDIA_TTL_SECONDS: i64 = 15 * 60;

#[derive(Clone, Copy)]
pub(crate) enum MediaFormat {
    Png,
    Jpeg,
    Webp,
    Mp4,
}

impl MediaFormat {
    fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Webp => "webp",
            Self::Mp4 => "mp4",
        }
    }

    fn is_video(self) -> bool {
        matches!(self, Self::Mp4)
    }

    fn valid(self, bytes: &[u8]) -> bool {
        let limit = if self.is_video() {
            MAX_VIDEO_BYTES
        } else {
            MAX_IMAGE_BYTES
        };
        bytes.len() <= limit
            && match self {
                Self::Png => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
                Self::Jpeg => bytes.starts_with(b"\xff\xd8\xff"),
                Self::Webp => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
                Self::Mp4 => bytes.get(4..8) == Some(b"ftyp") && bytes.len() >= 16,
            }
    }
}

pub(crate) struct Output {
    pub kind: String,
    pub name: String,
    pub bytes: Vec<u8>,
    pub synthesized: bool,
    pub ephemeral: bool,
}

pub(crate) async fn queue_media(
    db: &SqlitePool,
    vault: &Vault,
    task: &Task,
    action: &str,
    tool: &str,
    format: MediaFormat,
    bytes: &[u8],
) -> Result<bool> {
    ensure!(format.valid(bytes), "invalid or oversized output media");
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let (reply,): (Option<i64>,) = sqlx::query_as(
        "SELECT (SELECT e.telegram_message_id FROM events e WHERE e.task=t.id
                   AND e.role='user' AND e.telegram_message_id>0 ORDER BY e.id DESC LIMIT 1)
         FROM tasks t JOIN users u ON u.id=t.user AND u.suspended_at IS NULL
         WHERE t.id=? AND t.user=? AND t.revision=? AND t.state='running'
           AND t.agent IS NULL AND t.chief_chat=1 AND t.shared_memory_pair IS NULL AND t.schedule IS NULL
           AND NOT EXISTS(SELECT 1 FROM telegram_group_tasks g WHERE g.task=t.id)",
    )
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .fetch_optional(&mut *tx)
    .await?
    .context("private task is unavailable")?;
    let label = if format.is_video() {
        "recording"
    } else {
        "image"
    };
    let queued = queue(
        &mut tx,
        vault,
        &task.user,
        &task.id,
        task.revision,
        "document",
        &format!("{label}.{}", format.extension()),
        bytes,
        reply,
        None,
        Some(format),
    )
    .await?;
    super::requests::finish_action_in(
        &mut tx,
        task,
        action,
        &json!({"queued":queued,"media_delivery":tool}).to_string(),
        false,
    )
    .await?;
    tx.commit().await?;
    Ok(queued)
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
        body.as_bytes(),
        None,
        artifact.as_deref(),
        None,
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
    let Some(reply) = voice_reply(&mut tx, user, VoiceTarget::Exact(message), Some(outbox)).await?
    else {
        return Ok(false);
    };
    let queued = reply.enqueue(&mut tx, vault, user).await?;
    tx.commit().await?;
    Ok(queued)
}

pub(crate) enum VoiceTarget {
    Exact(i64),
    Before(i64),
}

pub(crate) struct VoiceNotices {
    pub request_message: i64,
    pub missing_reply: &'static str,
    pub missing_connection: &'static str,
}

pub(crate) async fn request_voice(
    db: &SqlitePool,
    vault: &Vault,
    user: &str,
    update: i64,
    target: Option<VoiceTarget>,
    notices: Option<VoiceNotices>,
) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let inserted = sqlx::query("INSERT OR IGNORE INTO updates VALUES('telegram',?)")
        .bind(update)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if inserted == 0 {
        return Ok(());
    }
    let reply = if let Some(target) = target {
        voice_reply(&mut tx, user, target, None).await?
    } else {
        None
    };
    let notice = match reply {
        Some(reply) if reply.configured => {
            reply.enqueue(&mut tx, vault, user).await?;
            None
        }
        Some(_) => notices.map(|notice| {
            (
                notice.request_message,
                notice.missing_connection,
                Some("mini"),
            )
        }),
        None => notices.map(|notice| (notice.request_message, notice.missing_reply, None)),
    };
    if let Some((message, text, button)) = notice {
        sqlx::query(
            "INSERT INTO outbox(user,body,button,reply_to_message_id)
             SELECT id,?,?,? FROM users WHERE id=? AND suspended_at IS NULL",
        )
        .bind(text)
        .bind(button)
        .bind(message)
        .bind(user)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct VoiceReply {
    task: String,
    revision: i64,
    body: String,
    message: i64,
    configured: bool,
}

impl VoiceReply {
    async fn enqueue(
        self,
        tx: &mut Transaction<'_, Sqlite>,
        vault: &Vault,
        user: &str,
    ) -> Result<bool> {
        let body = self.body.trim();
        if !self.configured || body.is_empty() {
            return Ok(false);
        }
        queue(
            tx,
            vault,
            user,
            &self.task,
            self.revision,
            "voice",
            "reply.mp3",
            crate::security::clip(body, 4_000).as_bytes(),
            Some(self.message),
            None,
            None,
        )
        .await
    }
}

async fn voice_reply(
    tx: &mut Transaction<'_, Sqlite>,
    user: &str,
    target: VoiceTarget,
    outbox: Option<i64>,
) -> Result<Option<VoiceReply>> {
    let (first, last) = match target {
        VoiceTarget::Exact(message) => (message, message),
        VoiceTarget::Before(message) => (1, message.saturating_sub(1)),
    };
    Ok(sqlx::query_as(
        "SELECT t.id AS task,t.revision,e.body,o.telegram_message_id AS message,
                (u.openrouter_key IS NOT NULL OR (u.provider IS 'openrouter' AND u.credential IS NOT NULL)) AS configured
         FROM outbox o JOIN events e ON e.id=o.assistant_event_id
         JOIN tasks t ON t.id=o.task AND t.id=e.task AND t.user=o.user
         JOIN users u ON u.id=t.user AND u.suspended_at IS NULL
         WHERE o.user=?1 AND o.telegram_message_id BETWEEN ?2 AND ?3 AND o.state='sent'
            AND (?4 IS NULL OR o.id=?4) AND e.role='assistant' AND trim(e.body)<>''
            AND t.agent IS NULL AND t.chief_chat=1 AND t.shared_memory_pair IS NULL
            AND t.state NOT IN ('cancelled','revoked')
         ORDER BY o.telegram_message_id DESC LIMIT 1",
    )
    .bind(user)
    .bind(first)
    .bind(last)
    .bind(outbox)
    .fetch_optional(&mut **tx)
    .await?)
}

async fn queue(
    tx: &mut Transaction<'_, Sqlite>,
    vault: &Vault,
    user: &str,
    task: &str,
    revision: i64,
    kind: &str,
    name: &str,
    content: &[u8],
    reply: Option<i64>,
    artifact: Option<&str>,
    media: Option<MediaFormat>,
) -> Result<bool> {
    let fingerprint = hex::encode(Sha256::digest(content));
    let format = media.map(MediaFormat::extension);
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM telegram_outputs f JOIN outbox o ON o.id=f.outbox
         WHERE o.task=? AND o.user=? AND f.revision=? AND f.kind=? AND f.name=? AND f.fingerprint=?
            AND f.media_format IS ?
            AND (o.state IN ('queued','sending','uncertain') OR (?='document' AND o.state='sent')))",
    )
    .bind(task)
    .bind(user)
    .bind(revision)
    .bind(kind)
    .bind(name)
    .bind(&fingerprint)
    .bind(format)
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
    let ciphertext = vault.seal(&binding(user, outbox), content)?;
    sqlx::query("INSERT INTO telegram_outputs(outbox,revision,kind,name,fingerprint,ciphertext,artifact,media_format,media_expires_at) VALUES(?,?,?,?,?,?,?,?,?)")
        .bind(outbox)
        .bind(revision)
        .bind(kind)
        .bind(name)
        .bind(fingerprint)
        .bind(ciphertext)
        .bind(artifact)
        .bind(format)
        .bind(media.map(|_| now() + MEDIA_TTL_SECONDS))
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
    let output: Option<(String, String, String, Option<String>, bool)> = sqlx::query_as(
        "SELECT coalesce(f.media_format,f.kind),f.name,f.ciphertext,f.audio,f.media_format IS NOT NULL
         FROM telegram_outputs f JOIN outbox o ON o.id=f.outbox
         JOIN tasks t ON t.id=o.task AND t.revision=f.revision AND t.user=o.user
         WHERE o.id=? AND o.user=? AND o.state='sending' AND t.agent IS NULL
           AND t.shared_memory_pair IS NULL
           AND EXISTS(SELECT 1 FROM users u WHERE u.id=t.user AND u.suspended_at IS NULL)
           AND t.state NOT IN ('cancelled','revoked') AND f.ciphertext IS NOT NULL
           AND (f.media_format IS NULL OR (t.chief_chat=1 AND t.schedule IS NULL AND f.media_expires_at>?
                AND NOT EXISTS(SELECT 1 FROM telegram_group_tasks g WHERE g.task=t.id)))",
    )
    .bind(item.id)
    .bind(&item.user)
    .bind(now())
    .fetch_optional(db)
    .await?;
    output
        .map(|(kind, name, ciphertext, audio, ephemeral)| {
            let bytes = match &audio {
                Some(audio) => vault.open(&audio_binding(item), audio)?,
                None => vault.open(&binding(&item.user, item.id), &ciphertext)?,
            };
            Ok(Output {
                kind,
                name,
                bytes,
                synthesized: audio.is_some(),
                ephemeral,
            })
        })
        .transpose()
}

pub(super) async fn prune_media(tx: &mut Transaction<'_, Sqlite>) -> Result<()> {
    sqlx::query(
        "UPDATE outbox SET state='failed',retry_at=0,body='',button=NULL
         WHERE state='queued' AND id IN (
             SELECT outbox FROM telegram_outputs WHERE media_expires_at<=?)",
    )
    .bind(now())
    .execute(&mut **tx)
    .await?;
    Ok(())
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
