use super::{
    Card, ChannelAdapter, Inbound, MessageDelivery, attachment, callback, voice, voice_request,
    widget,
};
use crate::{forms, security::now, store};
use anyhow::Result;
use sqlx::SqlitePool;
use std::{sync::Arc, time::Duration};
use store::outputs::{VoiceNotices, VoiceTarget};

const PROGRESS_SEND_TIMEOUT: Duration = Duration::from_secs(3);

enum Dispatch {
    Sent(Option<i64>),
    Retry(i64),
    Failed,
    Uncertain,
    Skipped,
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
                            if let Some(language) = voice_request::request(&text) {
                                let target = match reply {
                                    Some(reply) if reply.from_bot => Some(VoiceTarget::Exact(reply.message_id)),
                                    Some(_) => None,
                                    None => Some(VoiceTarget::Before(message_id)),
                                };
                                let (missing_reply, missing_connection) = match language {
                                    voice_request::Language::Dutch => (
                                        "Ik vind geen antwoord om voor te lezen. Reageer op het antwoord dat je wilt horen.",
                                        "Verbind OpenRouter in Instellingen om antwoorden als audio te krijgen.",
                                    ),
                                    voice_request::Language::English => (
                                        "I couldn’t find a reply to read aloud. Reply to the answer you want to hear.",
                                        "Connect OpenRouter in Settings to get audio replies.",
                                    ),
                                };
                                store::outputs::request_voice(db, &app.vault, &user, external_id, target, Some(VoiceNotices {
                                    request_message: message_id, missing_reply, missing_connection,
                                })).await?;
                                app.delivery_ready.notify_one();
                                continue;
                            }
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
                        continue;
                    }
                    Inbound::Callback(query) => {
                        if let Some((user, outbox, message)) = query.listen() {
                            let available = app.models.connections(&user).await?.openrouter_key;
                            let queued = available
                                && store::outputs::queue_voice(
                                    db, &app.vault, &user, outbox, message,
                                )
                                .await?;
                            if queued {
                                app.delivery_ready.notify_one();
                            }
                            let label = if queued {
                                "Preparing your voice reply."
                            } else if available {
                                "This voice reply is already queued or unavailable."
                            } else {
                                "Connect OpenRouter in Settings to listen."
                            };
                            let _ = adapter.answer_callback(&query.id, label).await;
                        } else {
                            handle_callback(db, &*adapter, &query, || app.wake_queues()).await?;
                        }
                        continue;
                    }
                    Inbound::Reaction {
                        external_id,
                        user,
                        message_id,
                        emoji,
                    } => {
                        if emoji.trim_end_matches('\u{fe0f}') == "🙉" {
                            store::outputs::request_voice(db, &app.vault, &user, external_id, Some(VoiceTarget::Exact(message_id)), None).await?;
                            app.delivery_ready.notify_one();
                            continue;
                        }
                        store::ingest_telegram_reaction(db, &operator, external_id, &user, message_id, &emoji).await?;
                    }
                }
                app.wake_queues();
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
            Some(format!(
                "{}{}{}",
                callback::INVITE_ACCEPT_PREFIX,
                callback::INVITE_INSIGHTS_SEGMENT,
                invitation,
            )),
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
    wake_queues: impl FnOnce(),
) -> Result<()> {
    let answer = callback::answer(db, query).await?;
    wake_queues();
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

pub async fn outbox(app: Arc<crate::App>, adapter: Arc<dyn ChannelAdapter>) {
    const DELIVERY_WORKERS: usize = 4;
    let mut jobs = tokio::task::JoinSet::new();
    loop {
        while jobs.len() < DELIVERY_WORKERS {
            let Ok(Some(item)) = store::claim_delivery(&app.db).await else {
                break;
            };
            let app = Arc::clone(&app);
            let adapter = Arc::clone(&adapter);
            jobs.spawn(async move { deliver(&app, &*adapter, &item).await });
        }
        tokio::select! {
            Some(result) = jobs.join_next(), if !jobs.is_empty() => {
                if result.is_err() {
                    tracing::warn!("an outbound delivery worker stopped unexpectedly");
                }
            },
            _ = app.delivery_ready.notified(), if jobs.len() < DELIVERY_WORKERS => {},
            _ = tokio::time::sleep(Duration::from_millis(500)), if jobs.len() < DELIVERY_WORKERS => {},
        }
    }
}

async fn deliver(app: &crate::App, adapter: &dyn ChannelAdapter, item: &store::Outbound) {
    let db = &app.db;
    let progress = item
        .button
        .as_deref()
        .is_some_and(|button| button.starts_with(store::acknowledgements::READY_PREFIX));
    let dispatch = match store::can_deliver(db, item).await {
        Ok(true) => match send_item(app, adapter, item).await {
            Ok(MessageDelivery::Sent(id)) => Dispatch::Sent(id),
            Ok(MessageDelivery::Retry(delay)) => Dispatch::Retry(delay),
            Ok(MessageDelivery::Skipped) => Dispatch::Skipped,
            Err(error)
                if error.is::<super::UploadUncertain>()
                    || error.is::<super::DeliveryUncertain>() =>
            {
                Dispatch::Uncertain
            }
            Err(_) => Dispatch::Failed,
        },
        Ok(false) => Dispatch::Skipped,
        Err(_) => Dispatch::Retry(2),
    };
    let (state, retry, message_id) = match dispatch {
        Dispatch::Sent(message_id) => ("sent", 0, message_id),
        Dispatch::Retry(_) if progress => ("skipped", 0, None),
        // Round the deadline up because the outbox clock has whole-second precision.
        Dispatch::Retry(delay) => ("queued", now() + delay + 1, None),
        Dispatch::Failed => ("failed", 0, None),
        Dispatch::Uncertain => ("uncertain", 0, None),
        Dispatch::Skipped => ("skipped", 0, None),
    };
    // On database failure, retry saving the outcome while holding the chat's
    // claim. The adapter send must never be repeated to recover this write.
    loop {
        let saved = sqlx::query(
            "UPDATE outbox SET state=?1,
                retry_at=?2,body=CASE WHEN ?1='queued' THEN body ELSE '' END,
                button=CASE WHEN ?1='queued' THEN button ELSE NULL END,
                widget=CASE WHEN ?1='queued' THEN widget ELSE NULL END,
                telegram_message_id=CASE WHEN NOT EXISTS(SELECT 1 FROM telegram_group_tasks g WHERE g.task=outbox.task) THEN ?3 END,
                telegram_group_message_id=CASE WHEN EXISTS(SELECT 1 FROM telegram_group_tasks g WHERE g.task=outbox.task) THEN ?3 END
             WHERE id=?4 AND state='sending'",
        )
        .bind(state)
        .bind(retry)
        .bind(message_id)
        .bind(item.id)
        .execute(db)
        .await;
        if saved.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
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
    if let Some(button) = button
        && let Some(allowed) = store::projects::notice_allowed(&app.db, &item.user, button).await?
    {
        if !allowed {
            return Ok(MessageDelivery::Skipped);
        }
        return adapter
            .send(&item.user, &item.body, Some("mini"), None)
            .await;
    }
    if let Some(token) = button.and_then(|button| button.strip_prefix(store::task_failures::RETRY))
    {
        let Some((mut message, retryable)) =
            store::task_failures::notice(&app.db, item, token).await?
        else {
            return Ok(MessageDelivery::Skipped);
        };
        if retryable {
            message.push_str("\n\nTap Retry to try again.");
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
            return Ok(MessageDelivery::Skipped);
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
            return Ok(MessageDelivery::Skipped);
        };
        if output.kind == "voice" && !output.synthesized {
            let text = std::str::from_utf8(&output.bytes)?;
            let audio = app.models.synthesize_reply(&item.user, text).await?;
            store::outputs::save_audio(&app.db, &app.vault, item, &audio).await?;
            let Some(current) = store::outputs::load(&app.db, &app.vault, item).await? else {
                return Ok(MessageDelivery::Skipped);
            };
            output = current;
        }
        let delivery = adapter
            .upload_file(
                &item.user,
                &output.kind,
                &output.name,
                &output.bytes,
                item.reply_to_message_id,
            )
            .await?;
        if output.ephemeral
            && matches!(delivery, MessageDelivery::Sent(id) if id.is_none_or(|id| id<=0))
        {
            return Err(super::UploadUncertain.into());
        }
        return Ok(delivery);
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
            None => Ok(MessageDelivery::Skipped),
        };
    }
    let button = item.button.as_deref();
    if button.is_some_and(|button| button.starts_with(store::acknowledgements::READY_PREFIX)) {
        if !store::acknowledgements::ready(db, item).await? {
            return Ok(MessageDelivery::Skipped);
        }
        return tokio::time::timeout(
            PROGRESS_SEND_TIMEOUT,
            adapter.send_plain(&item.user, &item.body, item.reply_to_message_id),
        )
        .await
        .map_err(|_| super::DeliveryUncertain)?;
    }
    if button.is_some_and(|button| button.starts_with("ta/") || button.starts_with("tq/")) {
        return Ok(MessageDelivery::Skipped);
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
