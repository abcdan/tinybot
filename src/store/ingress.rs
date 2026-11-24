use super::attachments::{self, SealedAttachment};
use crate::{
    audit::{self, ModelCall},
    security::{clip, hash, id, now},
};
use anyhow::Result;
use serde::Serialize;
use sqlx::{Sqlite, SqlitePool, Transaction};

#[cfg(test)]
mod tests;

#[derive(Clone, Serialize, sqlx::FromRow)]
pub struct ReplyContext {
    pub message_id: i64,
    pub excerpt: Option<String>,
    pub from_bot: bool,
}

#[derive(Default)]
struct TelegramInput<'a> {
    message_id: Option<i64>,
    reply: Option<ReplyContext>,
    literal: bool,
    reaction: bool,
    transcription: Option<(&'a ModelCall, u64)>,
    attachment: Option<SealedAttachment>,
}

pub async fn latest_reply_context(db: &SqlitePool, task: &str) -> Result<Option<ReplyContext>> {
    Ok(sqlx::query_as(
        "SELECT telegram_reply_message_id AS message_id,telegram_reply_excerpt AS excerpt,
             telegram_reply_from_bot AS from_bot
         FROM events WHERE id=(SELECT id FROM events WHERE task=? AND role='user' ORDER BY id DESC LIMIT 1)
           AND telegram_reply_message_id IS NOT NULL",
    )
    .bind(task)
    .fetch_optional(db)
    .await?)
}

pub async fn ingest(
    db: &SqlitePool,
    operator: &str,
    channel: &str,
    external_id: i64,
    user: &str,
    text: &str,
) -> Result<()> {
    ingest_message(
        db,
        operator,
        channel,
        external_id,
        user,
        text,
        TelegramInput::default(),
    )
    .await
}

#[cfg(test)]
pub async fn ingest_telegram(
    db: &SqlitePool,
    operator: &str,
    external_id: i64,
    user: &str,
    text: &str,
    message_id: i64,
) -> Result<()> {
    ingest_telegram_reply(db, operator, external_id, user, text, message_id, None).await
}

pub async fn ingest_telegram_reply(
    db: &SqlitePool,
    operator: &str,
    external_id: i64,
    user: &str,
    text: &str,
    message_id: i64,
    reply: Option<ReplyContext>,
) -> Result<()> {
    ingest_message(
        db,
        operator,
        "telegram",
        external_id,
        user,
        text,
        TelegramInput {
            message_id: (message_id > 0).then_some(message_id),
            reply,
            ..TelegramInput::default()
        },
    )
    .await
}

pub async fn ingest_telegram_voice(
    db: &SqlitePool,
    operator: &str,
    external_id: i64,
    user: &str,
    text: &str,
    message_id: i64,
    reply: Option<ReplyContext>,
    transcription: Option<(&ModelCall, u64)>,
) -> Result<()> {
    ingest_message(
        db,
        operator,
        "telegram",
        external_id,
        user,
        text,
        TelegramInput {
            message_id: (message_id > 0).then_some(message_id),
            reply,
            literal: true,
            transcription,
            ..TelegramInput::default()
        },
    )
    .await
}

pub(crate) async fn ingest_telegram_attachment(
    db: &SqlitePool,
    operator: &str,
    external_id: i64,
    user: &str,
    input: &str,
    message_id: i64,
    reply: Option<ReplyContext>,
    attachment: SealedAttachment,
) -> Result<()> {
    ingest_message(
        db,
        operator,
        "telegram",
        external_id,
        user,
        input,
        TelegramInput {
            message_id: Some(message_id),
            reply,
            literal: true,
            attachment: Some(attachment),
            ..TelegramInput::default()
        },
    )
    .await
}

pub async fn ingest_telegram_reaction(
    db: &SqlitePool,
    operator: &str,
    external_id: i64,
    user: &str,
    message_id: i64,
    emoji: &str,
) -> Result<()> {
    ingest_message(
        db,
        operator,
        "telegram",
        external_id,
        user,
        emoji,
        TelegramInput {
            message_id: None,
            reply: Some(ReplyContext {
                message_id,
                excerpt: None,
                from_bot: true,
            }),
            literal: true,
            reaction: true,
            ..TelegramInput::default()
        },
    )
    .await
}

async fn ingest_message(
    db: &SqlitePool,
    operator: &str,
    channel: &str,
    external_id: i64,
    user: &str,
    text: &str,
    mut telegram: TelegramInput<'_>,
) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let inserted = sqlx::query("INSERT OR IGNORE INTO updates VALUES (?,?)")
        .bind(channel)
        .bind(external_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if inserted == 0 {
        return Ok(());
    }
    let text = clip(text.trim(), 8000);
    let (command, argument) = text.split_once(' ').unwrap_or((text, ""));
    let command = if telegram.literal {
        ""
    } else {
        command.split('@').next().unwrap_or(command)
    };
    let argument = argument.trim();
    let mut response = None;
    if command == "/start" && !argument.is_empty() {
        let invite: Option<String> =
            sqlx::query_scalar("DELETE FROM invitations WHERE token=? AND expires>=? AND agent IS NOT NULL
                AND EXISTS(SELECT 1 FROM users WHERE id=? AND suspended_at IS NULL)
                AND EXISTS(SELECT 1 FROM users WHERE id=issuer AND suspended_at IS NULL) RETURNING agent")
                .bind(hash(argument))
                .bind(now())
                .bind(user)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(agent) = invite {
            sqlx::query("INSERT OR IGNORE INTO grants VALUES(?,?)")
                .bind(&agent)
                .bind(user)
                .execute(&mut *tx)
                .await?;
            let name: String = sqlx::query_scalar("SELECT name FROM agents WHERE id=?")
                .bind(&agent)
                .fetch_one(&mut *tx)
                .await?;
            response = Some(format!(
                "Invitation accepted. Ask your Chief to consult {name}."
            ));
        } else {
            response = Some("This invitation is invalid or expired.".into());
        }
    }
    let profile: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT t.id,u.provider FROM users u LEFT JOIN tasks t ON t.id=u.active_task
         AND t.user=u.id AND t.chief_chat=1
         AND (t.shared_memory_pair IS NULL OR (t.state<>'revoked' AND EXISTS(
              SELECT 1 FROM available_shared_memory_pairs p WHERE p.id=t.shared_memory_pair AND (p.owner=t.user OR p.guest=t.user))))
         WHERE u.id=? AND u.suspended_at IS NULL",
    )
    .bind(user)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((active_task, provider)) = profile else {
        tx.commit().await?;
        return Ok(());
    };
    let mut active = active_task;
    if response.is_none() {
        match command {
            "/start" | "/help" => response = Some("Message your Chief to start a task. Use /status to see its progress, /retry to retry a saved message when available, /new to start fresh, or /cancel to stop. Open /panel for tasks and schedules or /settings for account setup and forms. After checking an uncertain action, send /reconcile.".into()),
            "/panel" => response = Some("Manage TinyBot from this chat.".into()),
            "/settings" => response = Some("Open Settings & forms to manage your account and complete forms.".into()),
            "/invite" if user == operator => {
                let invite = super::invitations::create(&mut tx, operator).await?;
                response = Some(invite.command);
            }
            "/tasks" => {
                let rows = super::list_tasks(&mut *tx, user).await?;
                response = Some(if rows.is_empty() { "You have no tasks yet.".into() } else {
                    rows.into_iter().take(12).map(|task| {
                        if task.state == "cancelled" {
                            format!("{} ({})", task.title, task.state)
                        } else {
                            format!("{} ({})\n/switch {}", task.title, task.state, task.id)
                        }
                    }).collect::<Vec<_>>().join("\n\n")
                });
            }
            "/switch" => {
                let found = super::select_task(&mut *tx, user, argument).await?;
                response = Some(if found {"Switched tasks."} else {"Task unavailable."}.into());
            }
            "/status" => {
                super::task_views::status(&mut tx, user, active.as_deref(), telegram.reply.as_ref()).await?;
                tx.commit().await?;
                return Ok(());
            }
            "/retry" => {
                if super::task_failures::retry_active(&mut tx,user,active.as_deref(),telegram.reply.as_ref()).await? {
                    tx.commit().await?;
                    return Ok(());
                }
                response = Some("This task can’t be retried directly. Use /status to see what needs attention.".into());
            }
            "/approve_group" | "/disable_group" if user == operator => {
                let enabled = command == "/approve_group";
                let changed = super::groups::set_enabled(&mut tx, argument, enabled).await?;
                response = Some(match (enabled,changed) {
                    (true,true) => "Group enabled. Members can send an addressed /ask command there.",
                    (false,true) => "Group disabled.",
                    _ => "No matching group request or registration.",
                }.into());
            }
            "/cancel" => {
                let action_may_have_run: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tasks t WHERE t.id=? AND t.user=?
                    AND (t.state='uncertain' OR EXISTS(SELECT 1 FROM pending p WHERE p.task=t.id AND p.state='executing'
                        AND p.kind IN ('approve','form-submit','email-send'))))")
                    .bind(&active).bind(user).fetch_one(&mut *tx).await?;
                if action_may_have_run {
                    sqlx::query("INSERT INTO outbox(user,body) VALUES(?,?)")
                        .bind(user)
                        .bind("An action is in progress or its result is uncertain. Wait for the result, or check the destination and use /reconcile.")
                        .execute(&mut *tx).await?;
                    tx.commit().await?;
                    return Ok(());
                }
                if let Some(task) = active.as_deref() {
                    sqlx::query("UPDATE tasks SET state='cancelled',revision=revision+1 WHERE id=? AND user=? AND state<>'revoked'")
                        .bind(task).bind(user).execute(&mut *tx).await?;
                    crate::audit::cancel(&mut tx, task, user).await?;
                    sqlx::query("UPDATE pending SET state='superseded' WHERE task=? AND state IN ('waiting','approved')")
                        .bind(task).execute(&mut *tx).await?;
                    sqlx::query("UPDATE users SET active_task=NULL WHERE id=?").bind(user).execute(&mut *tx).await?;
                }
                response = Some("Active task stopped. An action already in flight may still report its result.".into());
            }
            "/new" if argument.is_empty() => {
                sqlx::query("UPDATE users SET active_task=NULL WHERE id=?").bind(user).execute(&mut *tx).await?;
                response = Some("Send a message to start your new task.".into());
            }
            "/new" => {
                active = None;
                telegram.reply = None;
            }
            "/reconcile" => {
                let resumed=sqlx::query("UPDATE tasks SET state='queued',revision=revision+1,updated=?
                    WHERE id=? AND user=? AND state='uncertain'").bind(now()).bind(&active).bind(user)
                    .execute(&mut *tx).await?.rows_affected()==1;
                if resumed {
                    sqlx::query("INSERT INTO events(task,role,body) VALUES(?,'user','I checked the external destination. Continue from here and do not automatically retry the earlier action.')")
                        .bind(&active).execute(&mut *tx).await?;
                }
                response=Some(if resumed {"Task resumed after reconciliation."} else {"The active task does not need reconciliation."}.into());
            }
            "/use" => response = Some("Message your Chief here. It can delegate to saved agents.".into()),
            _ if text.is_empty() => response = Some("Send a text message to give TinyBot a task.".into()),
            _ if command.starts_with('/') => response = Some("Use /help to see available commands.".into()),
            _ => {}
        }
    }
    if let Some(response) = response {
        let button = match command {
            "/start" | "/help" | "/panel" => Some("panel"),
            "/settings" => Some("mini"),
            "/invite" if user == operator => Some(super::invitations::BUTTON),
            _ => None,
        };
        sqlx::query("INSERT INTO outbox(user,body,button) VALUES(?,?,?)")
            .bind(user)
            .bind(response)
            .bind(button)
            .execute(&mut *tx)
            .await?;
    } else {
        let input = if command == "/new" { argument } else { text };
        let target: Option<(String, Option<String>, bool, bool)> = if command != "/new"
            && let Some(reply) = telegram
                .reply
                .as_ref()
                .filter(|reply| reply.from_bot && reply.message_id > 0)
        {
            sqlx::query_as(
                    "SELECT o.task,e.body,EXISTS(SELECT 1 FROM mail_messages m WHERE m.notice=o.id),
                            EXISTS(SELECT 1 FROM mail_messages m WHERE m.preview=o.id)
                     FROM outbox o LEFT JOIN events e ON e.id=o.assistant_event_id AND e.task=o.task
                     WHERE o.user=? AND o.telegram_message_id=? AND o.state='sent' AND o.task IS NOT NULL",
                )
                .bind(user).bind(reply.message_id).fetch_optional(&mut *tx).await?
        } else {
            None
        };
        if let Some((task, excerpt, mail_notice, mail_preview)) = target {
            if mail_notice && telegram.reaction {
                tx.commit().await?;
                return Ok(());
            }
            if !super::select_task(&mut *tx, user, &task).await? {
                if !telegram.reaction {
                    sqlx::query("INSERT INTO outbox(user,body) VALUES(?,?)")
                    .bind(user)
                    .bind("That message belongs to a task you can no longer continue. Send a new message to your Chief.")
                    .execute(&mut *tx)
                    .await?;
                }
                tx.commit().await?;
                return Ok(());
            }
            if let Some(reply) = &mut telegram.reply {
                if mail_preview {
                    reply.excerpt = None;
                } else if reply.excerpt.is_none() {
                    reply.excerpt = excerpt;
                }
            }
            if mail_notice && let Some(reply) = &telegram.reply {
                super::mail::reveal_reply(&mut tx, user, &task, reply.message_id).await?;
            }
            active = Some(task);
        } else if telegram.reaction {
            tx.commit().await?;
            return Ok(());
        }
        let (task, previous_state) = if let Some(active) = active {
            let state: Option<String> =
                sqlx::query_scalar("SELECT state FROM tasks WHERE id=? AND user=?")
                    .bind(&active)
                    .bind(user)
                    .fetch_optional(&mut *tx)
                    .await?;
            (active, state)
        } else {
            let task = id();
            sqlx::query("INSERT INTO tasks(id,user,title,goal,updated) VALUES(?,?,?,?,?)")
                .bind(&task)
                .bind(user)
                .bind(clip(input, 100))
                .bind(clip(input, 1500))
                .bind(now())
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE users SET active_task=? WHERE id=?")
                .bind(&task)
                .bind(user)
                .execute(&mut *tx)
                .await?;
            (task, None)
        };
        if !enqueue_input(
            &mut tx,
            user,
            &task,
            input,
            provider.is_some(),
            telegram.message_id,
            telegram.reply.as_ref(),
        )
        .await?
        {
            sqlx::query("INSERT INTO outbox(user,body) VALUES(?,?)")
                .bind(user)
                .bind("This task is unavailable. Use /new to start another task.")
                .execute(&mut *tx)
                .await?;
        } else {
            if let Some((usage, duration_ms)) = telegram.transcription {
                audit::ingress_model_call(
                    &mut tx,
                    &task,
                    "voice_transcription",
                    usage,
                    duration_ms,
                )
                .await?;
            }
            if let (Some(message_id), Some(attachment)) = (telegram.message_id, telegram.attachment)
            {
                attachments::save(&mut tx, &task, message_id, attachment).await?;
            }
            if let Some(message_id) = telegram.message_id.filter(|message_id| *message_id > 0) {
                super::task_views::acknowledge_input(
                    &mut tx,
                    user,
                    &task,
                    message_id,
                    matches!(previous_state.as_deref(), Some("queued" | "running")),
                )
                .await?;
            } else {
                super::task_views::notify_if_queued(
                    &mut tx,
                    user,
                    &task,
                    previous_state.as_deref() == Some("queued"),
                )
                .await?;
            }
        }
    }
    tx.commit().await?;
    Ok(())
}

// Record user text and invalidate older approvals under the caller's write transaction.
pub(super) async fn enqueue_input(
    tx: &mut Transaction<'_, Sqlite>,
    user: &str,
    task: &str,
    input: &str,
    has_provider: bool,
    telegram_message_id: Option<i64>,
    reply: Option<&ReplyContext>,
) -> Result<bool> {
    let state: Option<String> = sqlx::query_scalar(
        "UPDATE tasks SET state=CASE WHEN state IN ('running','uncertain') THEN state ELSE ? END,
        revision=revision+1,updated=? WHERE id=? AND user=? AND chief_chat=1 AND state NOT IN ('cancelled','revoked') AND
        EXISTS(SELECT 1 FROM users u WHERE u.id=tasks.user AND u.suspended_at IS NULL) AND
        (shared_memory_pair IS NULL OR EXISTS(SELECT 1 FROM available_shared_memory_pairs p WHERE p.id=tasks.shared_memory_pair AND (p.owner=tasks.user OR p.guest=tasks.user))) AND
        (agent IS NULL OR EXISTS(SELECT 1 FROM agents a WHERE a.id=tasks.agent AND
        (a.owner=? OR EXISTS(SELECT 1 FROM grants g WHERE g.agent=a.id AND g.member=?)))) RETURNING state",
    )
    .bind(if has_provider { "queued" } else { "setup" })
    .bind(now())
    .bind(task)
    .bind(user)
    .bind(user)
    .bind(user)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(state) = state else {
        return Ok(false);
    };
    sqlx::query(
        "UPDATE pending SET state='superseded' WHERE task=? AND state IN ('waiting','approved')",
    )
    .bind(task)
    .execute(&mut **tx)
    .await?;
    let reply = reply.filter(|reply| reply.message_id > 0);
    sqlx::query(
        "INSERT INTO events(task,role,body,telegram_message_id,telegram_reply_message_id,telegram_reply_excerpt,telegram_reply_from_bot)
         VALUES (?,'user',?,?,?,?,?)",
    )
        .bind(task)
        .bind(input)
        .bind(telegram_message_id)
        .bind(reply.map(|reply| reply.message_id))
        .bind(reply.and_then(|reply| reply.excerpt.as_deref()).map(|text| clip(text, 1600)))
        .bind(reply.map(|reply| reply.from_bot))
        .execute(&mut **tx)
        .await?;
    audit::open(tx, task, input).await?;
    if state == "uncertain" {
        sqlx::query("INSERT INTO outbox(user,body) VALUES(?,?)")
            .bind(user)
            .bind("I saved your message. Check the last action at its destination, then send /reconcile to continue.")
            .execute(&mut **tx)
            .await?;
    } else if !has_provider {
        sqlx::query("INSERT INTO outbox(user,body,button) VALUES(?,?,'mini')")
            .bind(user)
            .bind("I saved your task. Choose a model under Settings & forms and I’ll pick it up.")
            .execute(&mut **tx)
            .await?;
    }
    Ok(true)
}
