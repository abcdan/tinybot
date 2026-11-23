mod delivery;

pub(super) use delivery::reveal_reply;
pub use delivery::{
    MailAccess, MailContent, MailTaskContext, PollMailbox, advance_cursor, initialize_cursor,
    mail_preview, poll_mailboxes, record_delivery, reveal_mail, task_context,
};

use crate::security::{Vault, id, now};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
use std::collections::BTreeSet;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MailCredentials {
    pub host: String,
    pub port: u16,
    pub username: String,
    #[serde(default)]
    pub password: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FolderConfig {
    pub name: String,
    #[serde(default)]
    pub automate: bool,
    pub instruction: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MailConfig {
    pub label: String,
    pub address: String,
    pub imap: Option<MailCredentials>,
    pub smtp: Option<MailCredentials>,
    #[serde(default)]
    pub folders: Vec<FolderConfig>,
}

#[derive(Serialize)]
pub struct EndpointSummary {
    host: String,
    port: u16,
    username: String,
}

impl From<MailCredentials> for EndpointSummary {
    fn from(value: MailCredentials) -> Self {
        Self {
            host: value.host,
            port: value.port,
            username: value.username,
        }
    }
}

#[derive(Serialize)]
pub struct AccountSummary {
    id: String,
    label: String,
    address: String,
    imap: Option<EndpointSummary>,
    smtp: Option<EndpointSummary>,
    folders: Vec<FolderConfig>,
}

#[derive(FromRow, Serialize)]
pub struct SmtpAccount {
    pub id: String,
    pub label: String,
    pub address: String,
    pub epoch: i64,
}

pub struct SmtpConnection {
    pub epoch: i64,
    pub address: String,
    pub credentials: MailCredentials,
}

pub async fn list_accounts(
    db: &SqlitePool,
    vault: &Vault,
    owner: &str,
) -> Result<Vec<AccountSummary>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id,encrypted FROM mail_accounts WHERE owner=? ORDER BY label,id")
            .bind(owner)
            .fetch_all(db)
            .await?;
    rows.into_iter()
        .map(|(id, encrypted)| {
            let config = open_config(vault, owner, &id, &encrypted)?;
            Ok(AccountSummary {
                id,
                label: config.label,
                address: config.address,
                imap: config.imap.map(EndpointSummary::from),
                smtp: config.smtp.map(EndpointSummary::from),
                folders: config.folders,
            })
        })
        .collect()
}

pub async fn smtp_accounts(db: &SqlitePool, owner: &str) -> Result<Vec<SmtpAccount>> {
    Ok(sqlx::query_as("SELECT id,label,address,epoch FROM mail_accounts WHERE owner=? AND can_send=1 ORDER BY label,id")
        .bind(owner).fetch_all(db).await?)
}

pub async fn smtp_connection(
    db: &SqlitePool,
    vault: &Vault,
    owner: &str,
    account: &str,
) -> Result<SmtpConnection> {
    let (epoch, encrypted): (i64, String) = sqlx::query_as(
        "SELECT epoch,encrypted FROM mail_accounts WHERE id=? AND owner=? AND can_send=1",
    )
    .bind(account)
    .bind(owner)
    .fetch_optional(db)
    .await?
    .context("SMTP connection unavailable")?;
    let config = open_config(vault, owner, account, &encrypted)?;
    Ok(SmtpConnection {
        epoch,
        address: config.address,
        credentials: config.smtp.context("SMTP connection unavailable")?,
    })
}

pub async fn save_account(
    db: &SqlitePool,
    vault: &Vault,
    owner: &str,
    account: Option<&str>,
    mut config: MailConfig,
) -> Result<String> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let previous = if let Some(account) = account {
        let encrypted: String =
            sqlx::query_scalar("SELECT encrypted FROM mail_accounts WHERE id=? AND owner=?")
                .bind(account)
                .bind(owner)
                .fetch_optional(&mut *tx)
                .await?
                .context("Mail account unavailable")?;
        Some(open_config(vault, owner, account, &encrypted)?)
    } else {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mail_accounts WHERE owner=?")
            .bind(owner)
            .fetch_one(&mut *tx)
            .await?;
        ensure!(count < 10, "Too many mail accounts");
        None
    };
    normalize(&mut config, previous.as_ref())?;
    let reset_cursor = previous
        .as_ref()
        .is_some_and(|old| !same_mailbox(old.imap.as_ref(), config.imap.as_ref()));
    let id = account.map_or_else(id, str::to_owned);
    let encrypted = vault.seal(&account_binding(owner, &id), &serde_json::to_vec(&config)?)?;
    if account.is_some() {
        sqlx::query("UPDATE mail_accounts SET label=?,address=?,epoch=epoch+1,can_send=?,encrypted=? WHERE id=? AND owner=?")
            .bind(&config.label).bind(&config.address).bind(config.smtp.is_some()).bind(encrypted)
            .bind(&id).bind(owner).execute(&mut *tx).await?;
    } else {
        sqlx::query("INSERT INTO mail_accounts(id,owner,label,address,can_send,encrypted) VALUES(?,?,?,?,?,?)")
            .bind(&id).bind(owner).bind(&config.label).bind(&config.address).bind(config.smtp.is_some()).bind(encrypted)
            .execute(&mut *tx).await?;
    }
    let names: Vec<String> = config
        .folders
        .iter()
        .map(|folder| folder.name.clone())
        .collect();
    sqlx::query(
        "DELETE FROM mail_folders WHERE account=? AND name NOT IN (SELECT value FROM json_each(?))",
    )
    .bind(&id)
    .bind(serde_json::to_string(&names)?)
    .execute(&mut *tx)
    .await?;
    for folder in &config.folders {
        sqlx::query(
            "INSERT INTO mail_folders(account,name,automate) VALUES(?,?,?)
                     ON CONFLICT(account,name) DO UPDATE SET automate=excluded.automate,next_poll=0,
                     uid_validity=CASE WHEN ? THEN NULL ELSE uid_validity END,
                     last_uid=CASE WHEN ? THEN 0 ELSE last_uid END",
        )
        .bind(&id)
        .bind(&folder.name)
        .bind(folder.automate)
        .bind(reset_cursor)
        .bind(reset_cursor)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(id)
}

pub async fn remove_account(db: &SqlitePool, owner: &str, account: &str) -> Result<bool> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query(
        "UPDATE users SET active_task=NULL WHERE id=? AND active_task IN (
         SELECT m.task FROM mail_messages m JOIN mail_accounts a ON a.id=m.account WHERE a.id=? AND a.owner=?)",
    )
    .bind(owner)
    .bind(account)
    .bind(owner)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE tasks SET state='revoked',revision=revision+1,updated=? WHERE user=? AND id IN (
                 SELECT m.task FROM mail_messages m JOIN mail_accounts a ON a.id=m.account WHERE a.id=? AND a.owner=?)")
        .bind(now()).bind(owner).bind(account).bind(owner).execute(&mut *tx).await?;
    let removed = sqlx::query("DELETE FROM mail_accounts WHERE owner=? AND id=?")
        .bind(owner)
        .bind(account)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 1;
    tx.commit().await?;
    Ok(removed)
}

fn normalize(config: &mut MailConfig, previous: Option<&MailConfig>) -> Result<()> {
    config.label = config.label.trim().into();
    config.address = config.address.trim().into();
    ensure!(
        valid_label(&config.label, 80),
        "Enter a label for the mail account"
    );
    ensure!(
        valid_address(&config.address),
        "Enter a single email address"
    );
    ensure!(
        config.imap.is_some() || config.smtp.is_some(),
        "Configure IMAP or SMTP"
    );
    normalize_credentials(
        &mut config.imap,
        previous.and_then(|old| old.imap.as_ref()),
        false,
    )?;
    normalize_credentials(
        &mut config.smtp,
        previous.and_then(|old| old.smtp.as_ref()),
        true,
    )?;
    if config.imap.is_none() {
        config.folders.clear();
    } else if config.folders.is_empty() {
        config.folders.push(FolderConfig {
            name: "INBOX".into(),
            automate: false,
            instruction: None,
        });
    }
    ensure!(
        config.folders.len() <= 12,
        "Choose up to 12 monitored folders"
    );
    let mut seen = BTreeSet::new();
    for folder in &mut config.folders {
        folder.name = folder.name.trim().into();
        if folder.name.eq_ignore_ascii_case("INBOX") {
            folder.name = "INBOX".into();
        }
        ensure!(
            valid_label(&folder.name, 128) && seen.insert(&folder.name),
            "Enter unique IMAP folder names"
        );
        if let Some(instruction) = &folder.instruction {
            ensure!(
                instruction.len() <= 1500 && !instruction.contains('\0'),
                "The folder instruction is too long or invalid"
            );
        }
        folder.instruction = folder
            .instruction
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned);
    }
    Ok(())
}

fn normalize_credentials(
    field: &mut Option<MailCredentials>,
    previous: Option<&MailCredentials>,
    smtp: bool,
) -> Result<()> {
    let Some(credentials) = field else {
        return Ok(());
    };
    credentials.host = credentials
        .host
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    credentials.username = credentials.username.trim().to_owned();
    ensure!(
        credentials.host.len() <= 253
            && credentials.host.contains('.')
            && credentials.host.split('.').all(|part| !part.is_empty()
                && !part.starts_with('-')
                && !part.ends_with('-')
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')),
        "Enter a mail server hostname"
    );
    ensure!(
        valid_label(&credentials.username, 254),
        "Enter the mail account username"
    );
    ensure!(
        if smtp {
            matches!(credentials.port, 465 | 587)
        } else {
            credentials.port == 993
        },
        "Use IMAP port 993 and SMTP port 465 or 587"
    );
    if credentials.password.is_empty()
        && let Some(previous) = previous
        && credentials.host == previous.host
        && credentials.port == previous.port
        && credentials.username == previous.username
    {
        credentials.password = previous.password.clone();
    }
    ensure!(
        valid_label(&credentials.password, 512),
        "Enter the mail password or app password"
    );
    Ok(())
}

fn valid_label(text: &str, max: usize) -> bool {
    !text.is_empty() && text.len() <= max && !text.chars().any(char::is_control)
}

fn valid_address(addr: &str) -> bool {
    addr.len() <= 254
        && addr.is_ascii()
        && !addr
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        && addr.parse::<lettre::Address>().is_ok()
}

fn same_mailbox(left: Option<&MailCredentials>, right: Option<&MailCredentials>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => {
            left.host == right.host && left.port == right.port && left.username == right.username
        }
        (None, None) => true,
        _ => false,
    }
}

fn account_binding(owner: &str, account: &str) -> String {
    format!("mail-account:{owner}:{account}")
}

fn open_config(vault: &Vault, owner: &str, account: &str, encrypted: &str) -> Result<MailConfig> {
    let plain = vault.open(&account_binding(owner, account), encrypted)?;
    serde_json::from_slice(&plain).context("Invalid encrypted mail connection")
}
