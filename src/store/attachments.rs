use super::Task;
use crate::security::Vault;
use anyhow::{Result, ensure};
use sqlx::{Sqlite, SqlitePool, Transaction};

pub(crate) const MAX_ATTACHMENT_BYTES: usize = 8 * 1024 * 1024;

pub struct Attachment {
    pub event_id: i64,
    pub name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

pub(crate) struct SealedAttachment {
    name: String,
    mime: String,
    ciphertext: String,
}

pub(crate) fn seal(
    vault: &Vault,
    user: &str,
    message_id: i64,
    name: String,
    mime: &str,
    bytes: &[u8],
) -> Result<SealedAttachment> {
    ensure!(
        message_id > 0 && !bytes.is_empty() && bytes.len() <= MAX_ATTACHMENT_BYTES,
        "invalid Telegram attachment"
    );
    Ok(SealedAttachment {
        name,
        mime: mime.to_owned(),
        ciphertext: vault.seal(&binding(user, message_id), bytes)?,
    })
}

pub(super) async fn save(
    tx: &mut Transaction<'_, Sqlite>,
    task: &str,
    message_id: i64,
    attachment: SealedAttachment,
) -> Result<()> {
    let saved = sqlx::query(
        "INSERT INTO telegram_attachments(event,name,mime,ciphertext)
         SELECT id,?,?,? FROM events WHERE task=? AND role='user' AND telegram_message_id=?
         ORDER BY id DESC LIMIT 1",
    )
    .bind(attachment.name)
    .bind(attachment.mime)
    .bind(attachment.ciphertext)
    .bind(task)
    .bind(message_id)
    .execute(&mut **tx)
    .await?;
    ensure!(saved.rows_affected() == 1, "missing attachment event");
    Ok(())
}

pub async fn latest(db: &SqlitePool, vault: &Vault, task: &Task) -> Result<Option<Attachment>> {
    let row: Option<(i64, i64, String, String, String)> = sqlx::query_as(
        "WITH input AS (
            SELECT e.id,e.task,e.telegram_reply_message_id,e.telegram_reply_from_bot
            FROM tasks t JOIN events e ON e.task=t.id LEFT JOIN agents a ON a.id=t.agent
            WHERE t.id=? AND t.user=? AND t.revision=? AND t.agent IS ?
              AND t.state IN ('running','waiting') AND e.role='user'
              AND (t.agent IS NULL OR a.owner=t.user OR EXISTS (
                   SELECT 1 FROM grants g WHERE g.agent=t.agent AND g.member=t.user))
            ORDER BY e.id DESC LIMIT 1
         )
         SELECT e.id,e.telegram_message_id,f.name,f.mime,f.ciphertext FROM input i
         JOIN events e ON e.task=i.task AND e.role='user'
          AND (e.id=i.id OR (e.id<i.id AND i.telegram_reply_from_bot=0
                           AND e.telegram_message_id=i.telegram_reply_message_id))
         JOIN telegram_attachments f ON f.event=e.id ORDER BY e.id DESC LIMIT 1",
    )
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .bind(&task.agent)
    .fetch_optional(db)
    .await?;
    row.map(|(event_id, message_id, name, mime, ciphertext)| {
        Ok(Attachment {
            event_id,
            name,
            mime,
            bytes: vault.open(&binding(&task.user, message_id), &ciphertext)?,
        })
    })
    .transpose()
}

fn binding(user: &str, message_id: i64) -> String {
    format!("telegram-attachment:{user}:{message_id}")
}
