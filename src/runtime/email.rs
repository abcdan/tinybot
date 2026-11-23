use super::{Intent, Outcome, Risk, Tool, ToolContext, string};
use crate::{
    audit,
    mail::{self, SmtpSend},
    security::{Vault, hash, id, now},
    store::{
        self, Outbound, Pending,
        mail::{MailAccess, MailTaskContext},
    },
};
use anyhow::{Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::SqlitePool;

pub(super) const KIND: &str = "email-send";
const MAX_PREVIEW: usize = 3900;

#[derive(Debug)]
pub(super) struct DraftError(pub &'static str);

impl std::fmt::Display for DraftError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for DraftError {}

pub(super) struct SendEmail;

#[async_trait]
impl Tool for SendEmail {
    fn name(&self) -> &'static str {
        "send_email"
    }

    fn describe(&self) -> &'static str {
        "Prepare a plain-text email for this user's explicit approval. Use account from mail_accounts if needed. Set reply_to_current to reply in the current email thread."
    }

    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "account":{"type":"string","maxLength":64},
            "to":{"type":"array","maxItems":5,"items":{"type":"string","maxLength":254}},
            "subject":{"type":"string","maxLength":180},
            "body":{"type":"string","maxLength":2400},
            "reply_to_current":{"type":"boolean"}
        },"required":["subject","body"],"additionalProperties":false})
    }

    async fn inspect(&self, ctx: &ToolContext<'_>, args: &Value) -> Result<Intent> {
        ensure!(
            ctx.owner && ctx.parent_task.is_none() && ctx.task.agent.is_none(),
            "Only the user's Chief can prepare an email."
        );
        string(args, "subject", 180)?;
        string(args, "body", 2400)?;
        Ok(Intent {
            risk: Risk::Write,
            destination: "the configured mail server".into(),
            detail: "sending an email".into(),
        })
    }

    async fn execute(&self, _ctx: &ToolContext<'_>, _args: Value) -> Result<Outcome> {
        anyhow::bail!("Email must be sent from a single-use approval.");
    }
}

#[derive(Deserialize, Serialize)]
struct Approval {
    account: String,
    epoch: i64,
    fingerprint: String,
    sealed: String,
}

#[derive(Deserialize, Serialize)]
struct Draft {
    from: String,
    send: SmtpSend,
}

pub(super) async fn request(
    ctx: &ToolContext<'_>,
    args: &Value,
    email: Option<&MailTaskContext>,
) -> Result<bool> {
    ensure!(
        ctx.still_allows("send_email").await?,
        "Task changed before email approval."
    );
    let db = &ctx.app.db;
    let user = &ctx.task.user;
    let accounts = store::mail::smtp_accounts(db, user).await?;
    let explicit = args["account"].as_str();
    let reply = args["reply_to_current"].as_bool().unwrap_or(false);
    let original = if reply {
        Some(
            email
                .filter(|email| !matches!(email.access, MailAccess::Redacted))
                .and_then(|email| email.content.as_ref())
                .ok_or(DraftError(
                    "Open the email or reply to its Telegram notice first.",
                ))?,
        )
    } else {
        None
    };
    let account = explicit
        .and_then(|account| accounts.iter().find(|entry| entry.id == account))
        .or_else(|| {
            if explicit.is_some() {
                return None;
            }
            if reply
                && let Some(email) = email
                && let Some(account) = accounts.iter().find(|entry| entry.id == email.account)
            {
                return Some(account);
            }
            match accounts.as_slice() {
                [account] => Some(account),
                _ => None,
            }
        })
        .ok_or(DraftError(
            "Choose a configured SMTP account from mail_accounts.",
        ))?;
    let to = match args.get("to") {
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|_| DraftError("Provide recipient email addresses."))?,
        None => original
            .map(|mail| vec![mail.reply_to.as_ref().unwrap_or(&mail.from).clone()])
            .unwrap_or_default(),
    };
    let draft = Draft {
        from: account.address.clone(),
        send: SmtpSend {
            to,
            subject: string(args, "subject", 180)?.to_owned(),
            text: string(args, "body", 2400)?.to_owned(),
            in_reply_to: original.and_then(|mail| mail.message_id.clone()),
        },
    };
    draft.send.validate().map_err(|_| {
        DraftError(
            "Provide one to five valid email addresses, a short subject and a plain-text body.",
        )
    })?;
    render_preview(&draft)?;
    let request_id = id();
    let fingerprint = hash(&format!(
        "{}:{}:{}:{}",
        ctx.task.id,
        ctx.task.revision,
        account.id,
        serde_json::to_string(&draft)?
    ));
    let already_handled: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pending WHERE user=? AND task=? AND revision=? AND kind='email-send'
          AND state IN ('denied','done','uncertain','reconciled') AND json_extract(payload,'$.smtp_approval.fingerprint')=?)"
    ).bind(user).bind(&ctx.task.id).bind(ctx.task.revision).bind(&fingerprint).fetch_one(db).await?;
    if already_handled {
        store::append(
            db,
            &ctx.task.id,
            "tool",
            "This exact email was already handled or declined in this turn. Do not send it again.",
        )
        .await?;
        return Ok(false);
    }
    let approval = Approval {
        account: account.id.clone(),
        epoch: account.epoch,
        fingerprint,
        sealed: ctx
            .app
            .vault
            .seal(&binding(user, &request_id), &serde_json::to_vec(&draft)?)?,
    };
    store::pause(
        db,
        ctx.task,
        KIND,
        &json!({"smtp_approval":approval}),
        "Review the email draft before sending.",
        Some(&format!("mail-send/{request_id}")),
        900,
        &request_id,
    )
    .await?;
    Ok(true)
}

pub(super) async fn resume(
    ctx: &ToolContext<'_>,
    pending: &Pending,
    payload: &Value,
) -> Result<()> {
    let app = ctx.app;
    let user = &ctx.task.user;
    let Ok((approval, draft)) = open(&app.vault, user, &pending.id, payload) else {
        return mark_changed(ctx, pending).await;
    };
    let allowed = ctx.owner && ctx.task.agent.is_none() && ctx.still_allows("send_email").await?;
    let Ok(connection) =
        store::mail::smtp_connection(&app.db, &app.vault, user, &approval.account).await
    else {
        return mark_changed(ctx, pending).await;
    };
    if !allowed || connection.epoch != approval.epoch || connection.address != draft.from {
        return mark_changed(ctx, pending).await;
    }
    audit::record("approval", "send_email", "approved", Value::Null).await;
    audit::tool_started(
        "send_email",
        &json!({"recipient_count":draft.send.to.len()}),
    )
    .await;
    let result = mail::send(&connection, &draft.send, &pending.id)
        .await
        .map(|()| Outcome::Continue(json!({"email_submission":"accepted_for_delivery"})))
        .map_err(|_| anyhow::anyhow!("The mail server did not confirm delivery submission."));
    audit::tool_finished("send_email", &result).await;
    let uncertain = result.is_err();
    super::execution::finish(ctx, &pending.id, result, uncertain).await
}

async fn mark_changed(ctx: &ToolContext<'_>, pending: &Pending) -> Result<()> {
    store::finish_action(&ctx.app.db, ctx.task, &pending.id,
        "The SMTP connection or permissions changed. Nothing was sent. Prepare a new draft if the user still wants to send it.", false).await
}

pub(crate) fn smtp_preview(
    vault: &Vault,
    user: &str,
    request_id: &str,
    payload: &Value,
) -> Result<String> {
    let (_, draft) = open(vault, user, request_id, payload)?;
    render_preview(&draft)
}

pub(crate) async fn smtp_outbound_preview(
    db: &SqlitePool,
    vault: &Vault,
    item: &Outbound,
    request_id: &str,
) -> Result<Option<String>> {
    let payload: Option<String> = sqlx::query_scalar(
        "SELECT p.payload FROM pending p JOIN tasks t ON t.id=p.task AND t.revision=p.revision
         WHERE p.id=? AND p.user=? AND t.user=p.user AND p.task IS ? AND p.kind='email-send'
         AND p.state='waiting' AND p.expires>=? AND t.state='waiting' AND t.agent IS NULL
         AND EXISTS(SELECT 1 FROM mail_accounts m WHERE m.owner=p.user AND m.can_send=1
         AND m.id=json_extract(p.payload,'$.smtp_approval.account') AND m.epoch=json_extract(p.payload,'$.smtp_approval.epoch'))"
    ).bind(request_id).bind(&item.user).bind(&item.task).bind(now()).fetch_optional(db).await?;
    let Some(payload) = payload else {
        return Ok(None);
    };
    let payload = serde_json::from_str(&payload)?;
    smtp_preview(vault, &item.user, request_id, &payload).map(Some)
}

fn open(vault: &Vault, user: &str, request_id: &str, payload: &Value) -> Result<(Approval, Draft)> {
    let approval: Approval = serde_json::from_value(payload["smtp_approval"].clone())?;
    let draft: Draft =
        serde_json::from_slice(&vault.open(&binding(user, request_id), &approval.sealed)?)?;
    Ok((approval, draft))
}

fn binding(user: &str, request_id: &str) -> String {
    format!("smtp-draft:{user}:{request_id}")
}

fn render_preview(draft: &Draft) -> Result<String> {
    let text = format!(
        "Review this email before sending.\n\nFrom: {}\nTo: {}\nSubject: {}\n\n{}",
        draft.from,
        draft.send.to.join(", "),
        draft.send.subject,
        draft.send.text
    );
    ensure!(
        text.len() <= MAX_PREVIEW,
        DraftError(
            "Shorten the email or reduce its recipients so the full draft fits in the approval message."
        )
    );
    Ok(text)
}
