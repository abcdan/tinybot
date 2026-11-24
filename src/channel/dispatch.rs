use super::{Card, ChannelAdapter, Inbound, MessageDelivery, attachment, callback, voice, widget};
use crate::{forms, security::now, store};
use anyhow::Result;
use sqlx::SqlitePool;
use std::time::Duration;

enum Dispatch {
    Sent(Option<i64>),
    Retry(i64),
    Failed,
    Uncertain,
}

pub async fn listen(
    app: std::sync::Arc<crate::App>,
    operator: String,
    adapter: std::sync::Arc<dyn ChannelAdapter>,
) {
    let db = &app.db;
    let mut offset = 0;
    loop {
        let result = async {
            let (messages, next) = adapter.receive(offset).await?;
            for message in messages {
                match message {
                    Inbound::Message {
                        external_id,
                        user,
                        text,
                        telegram_message_id,
                        reply,
                    } => {
                        if let Some(message_id) = telegram_message_id {
                            if show_invitation(db, &*adapter, &operator, external_id, &user, &text)
                                .await?
                            {
                                continue;
                            }
                            store::ingest_telegram_reply(
                                db,
                                &operator,
                                external_id,
                                &user,
                                &text,
                                message_id,
                                reply,
                            )
                            .await?;
                        } else {
                            store::ingest(db, &operator, adapter.name(), external_id, &user, &text)
                                .await?;
                        }
                    }
                    Inbound::Voice(received) => {
                        voice::handle(&app, &operator, &*adapter, &received).await?;
                    }
                    Inbound::Group(message) => {
                        super::groups::handle(&app, &*adapter, message).await?
                    }
                    Inbound::Attachment(received) => {
                        attachment::handle(&app, &operator, &*adapter, received).await?;
                    }
                    Inbound::Widget(query) => {
                        let data = widget::answer(db, &query).await;
                        if adapter.answer_widget(&query.id, &data).await.is_err() {
                            tracing::debug!("widget answer expired or could not be delivered");
                        }
                    }
                    Inbound::Callback(query) => {
                        if let Some((user, outbox, message)) = query.listen() {
                            let available = app.models.connections(&user).await?.openrouter_key;
                            let queued = available
                                && store::outputs::queue_voice(
                                    db, &app.vault, &user, outbox, message,
                                )
                                .await?;
                            let label = if queued {
                                "Preparing your voice reply."
                            } else if available {
                                "This voice reply is already queued or unavailable."
                            } else {
                                "Connect OpenRouter in Settings to listen."
                            };
                            let _ = adapter.answer_callback(&query.id, label).await;
                        } else {
                            handle_callback(db, &*adapter, &query).await?;
                        }
                    }
                    Inbound::Reaction {
                        external_id,
                        user,
                        message_id,
                        emoji,
                    } => {
                        store::ingest_telegram_reaction(
                            db,
                            &operator,
                            external_id,
                            &user,
                            message_id,
                            &emoji,
                        )
                        .await?;
                    }
                }
            }
            Ok::<_, anyhow::Error>(next)
        }
        .await;
        match result {
            Ok(next) => offset = next,
            Err(error) => {
                tracing::warn!(channel=adapter.name(),reason=%error,"channel poll failed");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

pub(super) async fn show_invitation(
    db: &SqlitePool,
    adapter: &dyn ChannelAdapter,
    operator: &str,
    external_id: i64,
    user: &str,
    text: &str,
) -> Result<bool> {
    use store::invitations::{self, Review};

    let (command, code) = text.trim().split_once(' ').unwrap_or((text.trim(), ""));
    if command.split('@').next() != Some("/start") {
        return Ok(false);
    }
    let processed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM updates WHERE channel=? AND external_id=?)",
    )
    .bind(adapter.name())
    .bind(external_id)
    .fetch_one(db)
    .await?;
    if processed {
        return Ok(true);
    }
    let (body, button) = match invitations::review(db, operator, user, code.trim()).await? {
        Review::Available(invitation) => (
            invitations::REVIEW,
            Some(format!("{}{}", callback::INVITE_ACCEPT_PREFIX, invitation)),
        ),
        Review::Unavailable => (
            "This invitation is unavailable or has already been accepted.",
            None,
        ),
        Review::Other if !invitations::available(db, user).await? => (
            "Ask the bot operator for an invitation to TinyBot. A helper invitation requires an existing account.",
            None,
        ),
        Review::Other => return Ok(false),
    };
    if !matches!(
        adapter.send(user, body, button.as_deref(), None).await,
        Ok(MessageDelivery::Sent(_))
    ) {
        tracing::debug!("Telegram invitation review could not be delivered");
    }
    sqlx::query("INSERT OR IGNORE INTO updates VALUES(?,?)")
        .bind(adapter.name())
        .bind(external_id)
        .execute(db)
        .await?;
    Ok(true)
}

pub(super) async fn handle_callback(
    db: &SqlitePool,
    adapter: &dyn ChannelAdapter,
    query: &callback::Query,
) -> Result<()> {
    let answer = callback::answer(db, query).await?;
    if adapter
        .answer_callback(&query.id, answer.text)
        .await
        .is_err()
    {
        tracing::debug!("Telegram callback answer expired or could not be delivered");
    }
    if let Some((chat_id, message_id)) = answer.clear_keyboard
        && adapter
            .clear_inline_keyboard(chat_id, message_id)
            .await
            .is_err()
    {
        tracing::debug!("Telegram approval buttons could not be removed");
    }
    Ok(())
}

pub async fn outbox(app: std::sync::Arc<crate::App>, adapter: std::sync::Arc<dyn ChannelAdapter>) {
    let db = &app.db;
    loop {
        let result=async {
            let Some(item)=store::claim_outbox(db).await? else {return Ok(false);};
            let dispatch=if !store::can_deliver(db,&item).await? {
                Dispatch::Failed
            } else {
                let sent = send_item(&app, &*adapter, &item).await;
                match sent {
                    Ok(MessageDelivery::Sent(id))=>Dispatch::Sent(id),
                    Ok(MessageDelivery::Retry(delay))=>Dispatch::Retry(now()+delay),
                    Err(error) if error.is::<super::UploadUncertain>()=>Dispatch::Uncertain,
                    Err(_)=>Dispatch::Failed
                }
            };
            let (state,retry,message_id)=match dispatch {Dispatch::Sent(message_id)=>("sent",0,message_id), Dispatch::Retry(retry)=>("queued",retry,None), Dispatch::Failed=>("failed",0,None), Dispatch::Uncertain=>("uncertain",0,None)};
            sqlx::query("UPDATE outbox SET state=?,retry_at=?,body=CASE WHEN ?='queued' THEN body ELSE '' END,
                button=CASE WHEN ?='queued' THEN button ELSE NULL END,
                widget=CASE WHEN ?='queued' THEN widget ELSE NULL END,
                telegram_message_id=CASE WHEN NOT EXISTS(SELECT 1 FROM telegram_group_tasks g WHERE g.task=outbox.task) THEN ? END,
                telegram_group_message_id=CASE WHEN EXISTS(SELECT 1 FROM telegram_group_tasks g WHERE g.task=outbox.task) THEN ? END
                WHERE id=?")
                .bind(state).bind(retry).bind(state).bind(state).bind(state).bind(message_id).bind(message_id).bind(item.id).execute(db).await?;
            Ok::<_,anyhow::Error>(true)
        }.await;
        if !matches!(result, Ok(true)) {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

async fn send_item(
    app: &crate::App,
    adapter: &dyn ChannelAdapter,
    item: &store::Outbound,
) -> Result<MessageDelivery> {
    let button = item.button.as_deref();
    if let Some(route) = store::groups::destination(&app.db, item).await? {
        return adapter
            .send_group(route, &item.body, item.reply_to_message_id)
            .await;
    }
    if let Some(token) = button.and_then(|button| button.strip_prefix(store::task_failures::RETRY))
    {
        let Some((mut message, retryable)) =
            store::task_failures::notice(&app.db, item, token).await?
        else {
            return Ok(MessageDelivery::Sent(None));
        };
        if retryable {
            message.push_str("\n\nTap Retry to try your saved message again.");
        }
        return adapter
            .send(
                &item.user,
                &message,
                if retryable { button } else { Some("mini") },
                item.reply_to_message_id,
            )
            .await;
    }
    if button == Some(store::invitations::BUTTON) {
        let code = item.body.strip_prefix("/start ").unwrap_or_default();
        if item.user != app.operator
            || !store::invitations::shareable(&app.db, &item.user, code).await?
        {
            return Ok(MessageDelivery::Sent(None));
        }
        let body = if let Ok(Ok((_, username))) =
            tokio::time::timeout(Duration::from_secs(3), adapter.bot_identity()).await
            && (5..=32).contains(&username.len())
            && username
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == b'_')
        {
            format!(
                "Share this link with one guest. It expires after 24 hours.\n\nhttps://t.me/{username}?start={code}"
            )
        } else {
            format!(
                "Ask one guest to send this command to TinyBot. It expires after 24 hours.\n\n{}",
                item.body
            )
        };
        return adapter.send_plain(&item.user, &body, None).await;
    }
    if button == Some(store::outputs::BUTTON) {
        let Some(mut output) = store::outputs::load(&app.db, &app.vault, item).await? else {
            return Ok(MessageDelivery::Sent(None));
        };
        if output.kind == "voice" && !output.synthesized {
            let text = std::str::from_utf8(&output.bytes)?;
            let audio = app.models.synthesize_reply(&item.user, text).await?;
            store::outputs::save_audio(&app.db, &app.vault, item, &audio).await?;
            let Some(current) = store::outputs::load(&app.db, &app.vault, item).await? else {
                return Ok(MessageDelivery::Sent(None));
            };
            output = current;
        }
        return adapter
            .upload_file(
                &item.user,
                &output.kind,
                &output.name,
                &output.bytes,
                item.reply_to_message_id,
            )
            .await;
    }
    if let Some(message) = button.and_then(|button| button.strip_prefix("mail-preview/")) {
        let preview = store::mail::mail_preview(&app.db, &app.vault, item, message)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Email preview unavailable"))?;
        return adapter
            .send_plain(&item.user, &preview, item.reply_to_message_id)
            .await;
    }
    if let Some(request) = button.and_then(|button| button.strip_prefix(callback::MAIL_SEND_PREFIX))
    {
        let preview = crate::runtime::smtp_outbound_preview(&app.db, &app.vault, item, request)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Email approval unavailable"))?;
        return adapter
            .send(&item.user, &preview, button, item.reply_to_message_id)
            .await;
    }
    if button.is_none() && item.agent.is_none() && item.task.is_some()
        && sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM outbox o JOIN users u ON u.id=o.user JOIN tasks t ON t.id=o.task
                           WHERE o.id=? AND o.assistant_event_id IS NOT NULL AND u.openrouter_key IS NOT NULL
                             AND u.suspended_at IS NULL AND t.shared_memory_pair IS NULL)",
        )
        .bind(item.id)
        .fetch_one(&app.db)
        .await?
    {
        let replies = store::suggested_replies(&app.db, item.id).await?;
        let card = item.widget.as_deref().map(serde_json::from_str::<Card>).transpose()?;
        return adapter.send_listenable(&item.user, &item.body, card.as_ref(), &replies, item.reply_to_message_id, item.id).await;
    }
    send_outbound(&app.db, adapter, item).await
}

pub(super) async fn send_outbound(
    db: &SqlitePool,
    adapter: &dyn ChannelAdapter,
    item: &store::Outbound,
) -> Result<MessageDelivery> {
    if item.reaction_event.is_some() {
        return match store::reaction_target(db, item).await? {
            Some(message_id) => Ok(adapter
                .react(&item.user, message_id, &item.body)
                .await?
                .map_or(MessageDelivery::Sent(None), MessageDelivery::Retry)),
            None => Ok(MessageDelivery::Sent(None)),
        };
    }
    let button = item.button.as_deref();
    if button.is_some_and(|button| button.starts_with("ta/")) {
        let Some(task) = item.task.as_deref() else {
            return Ok(MessageDelivery::Sent(None));
        };
        let Some(controls) =
            store::task_views::acknowledgement_controls(db, &item.user, task, item.id).await?
        else {
            return Ok(MessageDelivery::Sent(None));
        };
        return adapter
            .send(
                &item.user,
                &item.body,
                Some(&controls),
                item.reply_to_message_id,
            )
            .await;
    }
    if let Some(raw) = button.and_then(|button| button.strip_prefix("tq/")) {
        let mut fields = raw.split('/');
        let task = fields.next().unwrap_or_default();
        let revision = fields.next().and_then(|value| value.parse().ok());
        if let Some(revision) = revision
            && !store::task_views::queued_notice_is_current(db, &item.user, task, revision).await?
        {
            return Ok(MessageDelivery::Sent(None));
        }
    }
    if let Some(request) = button.and_then(|button| button.strip_prefix("choice/")) {
        let choice = forms::widget_choice(db, &item.user, request).await?;
        return adapter.send_choice(&item.user, &choice).await;
    }
    let Ok(replies) = store::suggested_replies(db, item.id).await else {
        return Ok(MessageDelivery::Retry(2));
    };
    if let Some(widget) = &item.widget {
        let card: Card = serde_json::from_str(widget)?;
        adapter
            .send_card(&item.user, &card, &replies, item.reply_to_message_id)
            .await
    } else if replies.is_empty() {
        adapter
            .send(&item.user, &item.body, button, item.reply_to_message_id)
            .await
    } else {
        adapter
            .send_suggested_replies(&item.user, &item.body, &replies, item.reply_to_message_id)
            .await
    }
}
