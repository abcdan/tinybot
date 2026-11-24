use super::{MailCredentials, Vault, id, now, open_config};
use crate::{
    security::clip,
    store::{Outbound, Task, ingress::enqueue_input, select_task},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};

#[cfg(test)]
mod tests;

#[derive(FromRow)]
struct PollRow {
    owner: String,
    account: String,
    epoch: i64,
    folder: String,
    uid_validity: Option<u32>,
    last_uid: u32,
    encrypted: String,
}

pub struct PollMailbox {
    pub owner: String,
    pub account: String,
    pub epoch: i64,
    pub folder: String,
    pub imap: MailCredentials,
    pub uid_validity: Option<u32>,
    pub last_uid: u32,
    instruction: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub struct MailContent {
    pub from: String,
    pub reply_to: Option<String>,
    pub subject: String,
    pub text: String,
    pub message_id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MailAccess {
    Redacted,
    Automatic,
    Revealed,
}

#[derive(Serialize)]
pub struct MailTaskContext {
    pub account: String,
    pub account_label: String,
    pub folder: String,
    pub access: MailAccess,
    pub content: Option<MailContent>,
    pub automated_turn: bool,
}

pub async fn poll_mailboxes(db: &SqlitePool, vault: &Vault) -> Result<Vec<PollMailbox>> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let timestamp = now();
    let rows: Vec<PollRow> = sqlx::query_as(
        "SELECT a.owner,a.id AS account,a.epoch,f.name AS folder,f.uid_validity,f.last_uid,a.encrypted
         FROM mail_folders f JOIN mail_accounts a ON a.id=f.account
         JOIN users u ON u.id=a.owner AND u.suspended_at IS NULL
         WHERE f.next_poll<=? ORDER BY f.next_poll,f.account,f.name LIMIT 8",
    )
    .bind(timestamp)
    .fetch_all(&mut *tx)
    .await?;
    for row in &rows {
        sqlx::query("UPDATE mail_folders SET next_poll=? WHERE account=? AND name=?")
            .bind(timestamp + 60)
            .bind(&row.account)
            .bind(&row.folder)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    let mut mailboxes = Vec::new();
    for row in rows {
        let Ok(config) = open_config(vault, &row.owner, &row.account, &row.encrypted) else {
            tracing::warn!("Could not decrypt a saved IMAP connection");
            continue;
        };
        let Some(imap) = config.imap else { continue };
        let instruction = config
            .folders
            .into_iter()
            .find(|folder| folder.name == row.folder && folder.automate)
            .and_then(|folder| folder.instruction);
        mailboxes.push(PollMailbox {
            owner: row.owner,
            account: row.account,
            epoch: row.epoch,
            folder: row.folder,
            imap,
            uid_validity: row.uid_validity,
            last_uid: row.last_uid,
            instruction,
        });
    }
    Ok(mailboxes)
}

pub async fn initialize_cursor(
    db: &SqlitePool,
    mailbox: &PollMailbox,
    uid_validity: u32,
    last_uid: u32,
) -> Result<bool> {
    ensure!(
        uid_validity > 0 && mailbox.uid_validity != Some(uid_validity),
        "Invalid initial IMAP cursor"
    );
    set_cursor(db, mailbox, uid_validity, last_uid).await
}

pub async fn advance_cursor(
    db: &SqlitePool,
    mailbox: &PollMailbox,
    uid_validity: u32,
    last_uid: u32,
) -> Result<bool> {
    ensure!(
        uid_validity > 0
            && mailbox.uid_validity == Some(uid_validity)
            && last_uid >= mailbox.last_uid,
        "Invalid IMAP cursor advance"
    );
    set_cursor(db, mailbox, uid_validity, last_uid).await
}

async fn set_cursor(
    db: &SqlitePool,
    mailbox: &PollMailbox,
    uid_validity: u32,
    last_uid: u32,
) -> Result<bool> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let changed = update_cursor(&mut tx, mailbox, uid_validity, last_uid).await?;
    tx.commit().await?;
    Ok(changed)
}

async fn update_cursor(
    tx: &mut Transaction<'_, Sqlite>,
    mailbox: &PollMailbox,
    uid_validity: u32,
    last_uid: u32,
) -> Result<bool> {
    let updated = sqlx::query(
        "UPDATE mail_folders SET uid_validity=?,last_uid=? WHERE account=? AND name=?
         AND uid_validity IS ? AND last_uid=?
         AND EXISTS(SELECT 1 FROM mail_accounts a JOIN users u ON u.id=a.owner
             WHERE a.id=? AND a.owner=? AND a.epoch=? AND u.suspended_at IS NULL)",
    )
    .bind(uid_validity)
    .bind(last_uid)
    .bind(&mailbox.account)
    .bind(&mailbox.folder)
    .bind(mailbox.uid_validity)
    .bind(mailbox.last_uid)
    .bind(&mailbox.account)
    .bind(&mailbox.owner)
    .bind(mailbox.epoch)
    .execute(&mut **tx)
    .await?;
    Ok(updated.rows_affected() == 1)
}

pub async fn record_delivery(
    db: &SqlitePool,
    vault: &Vault,
    mailbox: &PollMailbox,
    uid_validity: u32,
    uid: u32,
    content: &MailContent,
) -> Result<bool> {
    ensure!(
        mailbox.uid_validity == Some(uid_validity) && uid_validity > 0 && uid > mailbox.last_uid,
        "Invalid IMAP message cursor"
    );
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    if !update_cursor(&mut tx, mailbox, uid_validity, uid).await? {
        return Ok(false);
    }
    let (label, automate, has_provider): (String, bool, bool) = sqlx::query_as(
        "SELECT a.label,f.automate,u.provider IS NOT NULL
         FROM mail_folders f JOIN mail_accounts a ON a.id=f.account JOIN users u ON u.id=a.owner
         WHERE a.id=? AND f.name=?",
    )
    .bind(&mailbox.account)
    .bind(&mailbox.folder)
    .fetch_one(&mut *tx)
    .await?;
    let task = id();
    let message = id();
    let encrypted = vault.seal(
        &message_binding(&mailbox.owner, &message),
        &serde_json::to_vec(content)?,
    )?;
    sqlx::query("INSERT INTO tasks(id,user,title,goal,state,updated) VALUES(?,?,?,?,'idle',?)")
        .bind(&task)
        .bind(&mailbox.owner)
        .bind(format!("Email · {label}"))
        .bind("Follow up on an incoming email.")
        .bind(now())
        .execute(&mut *tx)
        .await?;
    let (notice, automated_revision) = if automate && has_provider {
        ensure!(
            enqueue_input(
                &mut tx,
                &mailbox.owner,
                &task,
                mailbox
                    .instruction
                    .as_deref()
                    .unwrap_or("Summarize this new email and suggest a next step."),
                true,
                None,
                None,
            )
            .await?,
            "Unable to queue the email task"
        );
        let revision: i64 = sqlx::query_scalar("SELECT revision FROM tasks WHERE id=?")
            .bind(&task)
            .fetch_one(&mut *tx)
            .await?;
        (None, Some(revision))
    } else {
        let notice = sqlx::query("INSERT INTO outbox(user,task,body,button) VALUES(?,?,?,?)")
            .bind(&mailbox.owner)
            .bind(&task)
            .bind(format!(
                "New email in {label} ({}). Tap Read more or reply to this message to open it.",
                mailbox.folder
            ))
            .bind(format!("mail-read/{message}"))
            .execute(&mut *tx)
            .await?
            .last_insert_rowid();
        (Some(notice), None)
    };
    sqlx::query(
        "INSERT INTO mail_messages(id,account,account_epoch,folder,uid_validity,uid,task,notice,automated_revision,encrypted)
         VALUES(?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&message)
    .bind(&mailbox.account)
    .bind(mailbox.epoch)
    .bind(&mailbox.folder)
    .bind(uid_validity)
    .bind(uid)
    .bind(&task)
    .bind(notice)
    .bind(automated_revision)
    .bind(encrypted)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

#[derive(FromRow)]
struct ContextRow {
    id: String,
    account: String,
    account_label: String,
    folder: String,
    revealed: bool,
    automatic: bool,
    automated_revision: Option<i64>,
    encrypted: String,
}

pub async fn task_context(
    db: &SqlitePool,
    vault: &Vault,
    task: &Task,
) -> Result<Option<MailTaskContext>> {
    let row: Option<ContextRow> = sqlx::query_as(
        "SELECT m.id,m.account,a.label AS account_label,m.folder,m.revealed,
           (m.account_epoch=a.epoch AND coalesce(f.automate,0)=1) AS automatic,m.automated_revision,m.encrypted
         FROM mail_messages m JOIN mail_accounts a ON a.id=m.account JOIN tasks t ON t.id=m.task
         LEFT JOIN mail_folders f ON f.account=m.account AND f.name=m.folder
         WHERE t.id=? AND t.user=? AND a.owner=t.user AND t.revision=? AND t.state='running'
           AND t.chief_chat=1 AND t.agent IS NULL",
    )
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .fetch_optional(db)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let access = if row.revealed {
        MailAccess::Revealed
    } else if row.automatic && row.automated_revision.is_some() {
        MailAccess::Automatic
    } else {
        MailAccess::Redacted
    };
    let content = if matches!(&access, MailAccess::Redacted) {
        None
    } else {
        Some(open_message(vault, &task.user, &row.id, &row.encrypted)?)
    };
    Ok(Some(MailTaskContext {
        account: row.account,
        account_label: row.account_label,
        folder: row.folder,
        access,
        content,
        automated_turn: row.automated_revision == Some(task.revision),
    }))
}

/// Reads a bounded digest from folders the user has enabled for automatic handling.
pub async fn briefing(db: &SqlitePool, vault: &Vault, task: &Task) -> Result<Vec<MailContent>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT m.id,m.encrypted FROM mail_messages m
         JOIN mail_accounts a ON a.id=m.account AND a.epoch=m.account_epoch
         JOIN mail_folders f ON f.account=a.id AND f.name=m.folder
         JOIN tasks t ON t.id=? AND t.user=a.owner AND t.revision=? AND t.state='running'
         JOIN schedule_runs r ON r.task=t.id AND r.source='inbox'
         JOIN users u ON u.id=t.user AND u.suspended_at IS NULL
         WHERE a.owner=? AND f.automate=1 AND m.automated_revision IS NOT NULL
           AND t.agent IS NULL AND t.shared_memory_pair IS NULL
         ORDER BY m.rowid DESC LIMIT 12",
    )
    .bind(&task.id)
    .bind(task.revision)
    .bind(&task.user)
    .fetch_all(db)
    .await?;
    rows.into_iter()
        .map(|(id, encrypted)| {
            let mut message = open_message(vault, &task.user, &id, &encrypted)?;
            message.text = clip(&message.text, 700).to_owned();
            message.reply_to = None;
            message.message_id = None;
            Ok(message)
        })
        .collect()
}

pub async fn reveal_mail(
    db: &SqlitePool,
    user: &str,
    message: &str,
    telegram_message_id: i64,
) -> Result<bool> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let task: Option<String> = sqlx::query_scalar(
        "SELECT m.task FROM mail_messages m JOIN mail_accounts a ON a.id=m.account
         JOIN outbox o ON o.id=m.notice AND o.task=m.task AND o.user=a.owner
         WHERE m.id=? AND a.owner=? AND m.preview IS NULL AND o.telegram_message_id=? AND o.state='sent'",
    )
    .bind(message)
    .bind(user)
    .bind(telegram_message_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(task) = task else { return Ok(false) };
    if !select_task(&mut *tx, user, &task).await? {
        return Ok(false);
    }
    let preview = sqlx::query(
        "INSERT INTO outbox(user,task,body,button,reply_to_message_id) VALUES(?,?,'',?,?)",
    )
    .bind(user)
    .bind(&task)
    .bind(format!("mail-preview/{message}"))
    .bind(telegram_message_id)
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    sqlx::query("UPDATE mail_messages SET revealed=1,preview=? WHERE id=?")
        .bind(preview)
        .bind(message)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

pub(in crate::store) async fn reveal_reply(
    tx: &mut Transaction<'_, Sqlite>,
    user: &str,
    task: &str,
    telegram_message_id: i64,
) -> Result<()> {
    sqlx::query(
        "UPDATE mail_messages SET revealed=1 WHERE task=? AND revealed=0
         AND EXISTS(SELECT 1 FROM mail_accounts WHERE id=mail_messages.account AND owner=?)
         AND EXISTS(SELECT 1 FROM outbox WHERE id=mail_messages.notice AND task=mail_messages.task
           AND state='sent' AND telegram_message_id=? AND user=?)",
    )
    .bind(task)
    .bind(user)
    .bind(telegram_message_id)
    .bind(user)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn mail_preview(
    db: &SqlitePool,
    vault: &Vault,
    item: &Outbound,
    message: &str,
) -> Result<Option<String>> {
    let encrypted: Option<String> = sqlx::query_scalar(
        "SELECT m.encrypted FROM mail_messages m JOIN mail_accounts a ON a.id=m.account
         JOIN tasks t ON t.id=m.task AND t.user=a.owner
         WHERE m.id=? AND m.preview=? AND m.task=? AND a.owner=? AND m.revealed=1
           AND t.state NOT IN ('cancelled','revoked') AND t.agent IS NULL AND t.chief_chat=1",
    )
    .bind(message)
    .bind(item.id)
    .bind(&item.task)
    .bind(&item.user)
    .fetch_optional(db)
    .await?;
    let Some(encrypted) = encrypted else {
        return Ok(None);
    };
    let content = open_message(vault, &item.user, message, &encrypted)?;
    let text = if content.text.is_empty() {
        "(No text body)"
    } else {
        &content.text
    };
    let snippet = clip(text, 2800);
    let ellipsis = if snippet.len() < text.len() {
        "…"
    } else {
        ""
    };
    Ok(Some(format!(
        "From: {}\nSubject: {}\n\n{snippet}{ellipsis}\n\nReply to this message or send a follow-up to ask TinyBot about this email.",
        clip(&content.from, 400),
        clip(&content.subject, 400),
    )))
}

fn message_binding(user: &str, message: &str) -> String {
    format!("mail-message:{user}:{message}")
}

fn open_message(vault: &Vault, user: &str, message: &str, encrypted: &str) -> Result<MailContent> {
    let plain = vault.open(&message_binding(user, message), encrypted)?;
    serde_json::from_slice(&plain).context("Invalid encrypted mail content")
}
