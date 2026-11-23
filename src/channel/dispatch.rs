use super::{Card, ChannelAdapter, Inbound, MessageDelivery, callback, voice, widget};
use crate::{forms, security::now, store};
use anyhow::Result;
use sqlx::SqlitePool;
use std::time::Duration;

enum Dispatch {
    Sent(Option<i64>),
    Retry(i64),
    Failed,
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
                    Inbound::Widget(query) => {
                        let data = widget::answer(db, &query).await;
                        if adapter.answer_widget(&query.id, &data).await.is_err() {
                            tracing::debug!("widget answer expired or could not be delivered");
                        }
                    }
                    Inbound::Callback(query) => {
                        let text = callback::answer(db, &query).await?;
                        if adapter.answer_callback(&query.id, text).await.is_err() {
                            tracing::debug!("callback answer expired or could not be delivered");
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

pub async fn outbox(db: SqlitePool, adapter: std::sync::Arc<dyn ChannelAdapter>) {
    loop {
        let result=async {
            let Some(item)=store::claim_outbox(&db).await? else {return Ok(false);};
            let dispatch=if !store::can_deliver(&db,&item).await? {
                Dispatch::Failed
            } else {
                let sent = send_outbound(&db, &*adapter, &item).await;
                match sent {
                    Ok(MessageDelivery::Sent(id))=>Dispatch::Sent(id),
                    Ok(MessageDelivery::Retry(delay))=>Dispatch::Retry(now()+delay),
                    Err(_)=>Dispatch::Failed
                }
            };
            let (state,retry,message_id)=match dispatch {Dispatch::Sent(message_id)=>("sent",0,message_id), Dispatch::Retry(retry)=>("queued",retry,None), Dispatch::Failed=>("failed",0,None)};
            sqlx::query("UPDATE outbox SET state=?,retry_at=?,body=CASE WHEN ?='queued' THEN body ELSE '' END,
                button=CASE WHEN ?='queued' THEN button ELSE NULL END,
                widget=CASE WHEN ?='queued' THEN widget ELSE NULL END,telegram_message_id=? WHERE id=?")
                .bind(state).bind(retry).bind(state).bind(state).bind(state).bind(message_id).bind(item.id).execute(&db).await?;
            Ok::<_,anyhow::Error>(true)
        }.await;
        if !matches!(result, Ok(true)) {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
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
