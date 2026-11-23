mod callback;
mod card;
mod formatting;
mod voice;
mod widget;

pub use card::Card;

use crate::{
    forms::{self, WidgetChoice},
    net,
    security::{clip, now},
    store,
};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use std::time::Duration;

const PANEL_HTML: &str = include_str!("../web/telegram-widget.html");
const CHOICE_HTML: &str = include_str!("../web/telegram-choice.html");

pub enum Inbound {
    Message {
        external_id: i64,
        user: String,
        text: String,
        telegram_message_id: Option<i64>,
    },
    Voice(voice::Received),
    Widget(widget::Query),
    Callback(callback::Query),
}

enum Dispatch {
    Sent,
    Retry(i64),
    Failed,
}

enum WidgetDelivery {
    Sent,
    Retry(i64),
    Rejected,
}

#[async_trait]
pub trait ChannelAdapter: Send + Sync {
    fn name(&self) -> &'static str;
    async fn receive(&self, offset: i64) -> Result<(Vec<Inbound>, i64)>;
    async fn send(&self, user: &str, body: &str, button: Option<&str>) -> Result<Option<i64>>;
    async fn download_voice(&self, _file_id: &str) -> Result<Vec<u8>> {
        anyhow::bail!("channel does not support voice downloads")
    }

    async fn answer_widget(&self, _query: &str, _data: &str) -> Result<()> {
        Ok(())
    }

    async fn send_suggested_replies(
        &self,
        user: &str,
        body: &str,
        _replies: &[store::SuggestedReply],
    ) -> Result<Option<i64>> {
        self.send(user, body, None).await
    }

    async fn answer_callback(&self, _query: &str, _text: &str) -> Result<()> {
        Ok(())
    }

    async fn send_card(
        &self,
        user: &str,
        card: &Card,
        replies: &[store::SuggestedReply],
    ) -> Result<Option<i64>> {
        self.send_suggested_replies(user, &card.plain_text(), replies)
            .await
    }

    async fn send_choice(&self, user: &str, choice: &WidgetChoice) -> Result<Option<i64>> {
        self.send(user, &choice.label, Some("mini")).await
    }

    async fn typing(&self, _user: &str) -> Result<Option<Duration>> {
        Ok(None)
    }

    async fn draft(&self, _user: &str, _id: i32, _text: &str) -> Result<Option<Duration>> {
        Ok(None)
    }

    async fn react(&self, _user: &str, _message_id: i64, _emoji: &str) -> Result<Option<i64>> {
        anyhow::bail!("channel does not support reactions")
    }
}

pub struct Telegram {
    client: reqwest::Client,
    api: String,
    origin: String,
}

impl Telegram {
    pub fn new(token: String, origin: String) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(45))
                .build()?,
            api: format!("https://api.telegram.org/bot{token}"),
            origin,
        })
    }

    async fn feedback(&self, method: &str, data: Value) -> Result<Option<Duration>> {
        let output = tokio::time::timeout(Duration::from_secs(3), async {
            let mut data = data;
            let output = self.call(method, data.clone()).await?;
            if data.get("entities").is_some()
                && bad_request(&output).is_some_and(|description| invalid_entities(&description))
            {
                data.as_object_mut()
                    .expect("feedback object")
                    .remove("entities");
                return self.call(method, data).await;
            }
            Ok(output)
        })
        .await??;
        if output["ok"] == true {
            return Ok(None);
        }
        if let Some(seconds) = output["parameters"]["retry_after"].as_u64() {
            return Ok(Some(Duration::from_secs(seconds.clamp(1, 600))));
        }
        anyhow::bail!("Telegram rejected the chat feedback")
    }

    async fn call(&self, method: &str, data: Value) -> Result<Value> {
        let response = self
            .client
            .post(format!("{}/{method}", self.api))
            .json(&data)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Telegram request failed"))?;
        Ok(serde_json::from_slice(
            &net::body(response, 2_000_000).await?,
        )?)
    }

    async fn send_widget(&self, kind: &str, data: Value) -> Result<WidgetDelivery> {
        let response = self.call("sendWidget", data).await?;
        if response["ok"] == true {
            tracing::debug!(
                widget_kind = kind,
                has_widget = response["result"]["widget"].is_object(),
                "Telegram accepted widget message"
            );
            return Ok(WidgetDelivery::Sent);
        }
        if let Some(delay) = retry_after(&response) {
            return Ok(WidgetDelivery::Retry(delay));
        }
        let error_code = response["error_code"].as_i64();
        let category = widget_rejection(&response);
        tracing::warn!(
            widget_kind = kind,
            ?error_code,
            category,
            "Telegram rejected sendWidget"
        );
        Ok(WidgetDelivery::Rejected)
    }

    async fn send_message(&self, data: Value) -> Result<Option<i64>> {
        message_result(&self.call("sendMessage", data).await?)
    }

    async fn send_reply(
        &self,
        user: i64,
        body: &str,
        keyboard: Option<Value>,
    ) -> Result<Option<i64>> {
        let reply = formatting::render(body);
        let mut data =
            json!({"chat_id":user,"text":reply.text,"link_preview_options":{"is_disabled":true}});
        if let Some(keyboard) = &keyboard {
            data["reply_markup"] = keyboard.clone();
        }
        let has_entities = !reply.entities.is_empty();
        if has_entities {
            data["entities"] = json!(reply.entities);
        }
        let has_links = !reply.links.is_empty();
        if has_links {
            let mut markup = keyboard
                .clone()
                .unwrap_or_else(|| json!({"inline_keyboard":[]}));
            if let Some(rows) = markup["inline_keyboard"].as_array_mut() {
                rows.extend(reply.links.iter().map(|link| json!([link])));
            }
            data["reply_markup"] = markup;
        }
        let response = self.call("sendMessage", data.clone()).await?;
        if let Some(description) = bad_request(&response) {
            if has_entities && invalid_entities(&description) {
                data.as_object_mut()
                    .expect("message object")
                    .remove("entities");
                return self.send_message(data).await;
            }
            if has_links
                && (description.contains("button_url_invalid")
                    || description.contains("button url"))
            {
                if let Some(keyboard) = keyboard {
                    data["reply_markup"] = keyboard;
                } else {
                    data.as_object_mut()
                        .expect("message object")
                        .remove("reply_markup");
                }
                return self.send_message(data).await;
            }
        }
        message_result(&response)
    }
}

fn message_result(response: &Value) -> Result<Option<i64>> {
    if response["ok"] == true {
        return Ok(None);
    }
    if let Some(delay) = retry_after(response) {
        return Ok(Some(delay));
    }
    anyhow::bail!("Telegram rejected the reply")
}

fn bad_request(response: &Value) -> Option<String> {
    (response["ok"] == false && response["error_code"] == 400 && retry_after(response).is_none())
        .then(|| {
            response["description"]
                .as_str()
                .unwrap_or("")
                .to_ascii_lowercase()
        })
}

fn invalid_entities(description: &str) -> bool {
    description.contains("parse entities") || description.contains("entity bounds")
}

fn retry_after(response: &Value) -> Option<i64> {
    response["parameters"]["retry_after"]
        .as_i64()
        .map(|delay| delay.clamp(1, 600))
}

fn widget_rejection(response: &Value) -> &'static str {
    let description = response["description"]
        .as_str()
        .unwrap_or("")
        .to_ascii_lowercase();
    if description.contains("input_media_widget_not_allowed") {
        "widget_access_restricted"
    } else if description.contains("widget_html") || description.contains("widget_too_large") {
        "invalid_widget_html"
    } else if description.contains("widget_data") {
        "invalid_widget_data"
    } else if description.contains("button") || description.contains("reply markup") {
        "invalid_widget_keyboard"
    } else if response["error_code"] == 404 {
        "method_unavailable"
    } else {
        "other"
    }
}

fn suggested_keyboard(replies: &[store::SuggestedReply]) -> Value {
    let buttons: Vec<_> = replies
        .iter()
        .map(|reply| [json!({"text":reply.body,"callback_data":format!("reply/{}",reply.token)})])
        .collect();
    json!({"inline_keyboard":buttons})
}

#[derive(Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Message>,
    widget_query: Option<widget::Query>,
    callback_query: Option<callback::Query>,
}
#[derive(Deserialize)]
struct Message {
    message_id: i64,
    chat: Chat,
    from: Option<User>,
    text: Option<String>,
    voice: Option<voice::Note>,
}
#[derive(Deserialize)]
struct Chat {
    id: i64,
    #[serde(rename = "type")]
    kind: String,
}
#[derive(Deserialize)]
struct User {
    id: i64,
    is_bot: bool,
}

impl Chat {
    fn is_private_with(&self, user: &User) -> bool {
        self.kind == "private" && self.id == user.id && !user.is_bot
    }
}

#[async_trait]
impl ChannelAdapter for Telegram {
    fn name(&self) -> &'static str {
        "telegram"
    }

    async fn typing(&self, user: &str) -> Result<Option<Duration>> {
        let user = user.parse::<i64>().context("invalid Telegram recipient")?;
        self.feedback("sendChatAction", json!({"chat_id":user,"action":"typing"}))
            .await
    }

    async fn draft(&self, user: &str, id: i32, text: &str) -> Result<Option<Duration>> {
        let user = user.parse::<i64>().context("invalid Telegram recipient")?;
        let reply = formatting::render(text);
        let mut data = json!({"chat_id":user,"draft_id":id,"text":reply.text});
        if !reply.entities.is_empty() {
            data["entities"] = json!(reply.entities);
        }
        self.feedback("sendMessageDraft", data).await
    }

    async fn react(&self, user: &str, message_id: i64, emoji: &str) -> Result<Option<i64>> {
        let user = user.parse::<i64>().context("invalid Telegram recipient")?;
        let retry = self
            .feedback(
                "setMessageReaction",
                json!({"chat_id":user,"message_id":message_id,"reaction":[{"type":"emoji","emoji":emoji}]}),
            )
            .await?;
        Ok(retry.map(|delay| delay.as_secs() as i64))
    }

    async fn download_voice(&self, file_id: &str) -> Result<Vec<u8>> {
        voice::download(self, file_id).await
    }

    async fn receive(&self, offset: i64) -> Result<(Vec<Inbound>, i64)> {
        let output = self
            .call(
                "getUpdates",
                json!({"offset":offset,"timeout":25,"allowed_updates":["message","widget_query","callback_query"]}),
            )
            .await?;
        ensure!(
            output["ok"] == true,
            "Telegram polling failed. Check the bot token and webhook configuration."
        );
        let updates: Vec<Update> = serde_json::from_value(output["result"].clone())?;
        let offset = updates.last().map_or(offset, |last| last.update_id + 1);
        let messages = updates
            .into_iter()
            .filter_map(|update| {
                if let Some(query) = update.widget_query {
                    return query.is_private().then_some(Inbound::Widget(query));
                }
                if let Some(query) = update.callback_query {
                    return query.is_private().then_some(Inbound::Callback(query));
                }
                let message = update.message?;
                let user = message.from?;
                if !message.chat.is_private_with(&user) {
                    return None;
                }
                if let Some(note) = message.voice {
                    return Some(Inbound::Voice(voice::Received::new(
                        update.update_id,
                        user.id.to_string(),
                        message.message_id,
                        note,
                    )));
                }
                Some(Inbound::Message {
                    external_id: update.update_id,
                    user: user.id.to_string(),
                    text: message.text.unwrap_or_default(),
                    telegram_message_id: Some(message.message_id),
                })
            })
            .collect();
        Ok((messages, offset))
    }

    async fn send(&self, user: &str, body: &str, button: Option<&str>) -> Result<Option<i64>> {
        let user = user.parse::<i64>().context("invalid Telegram recipient")?;
        if button.is_none() {
            return self.send_reply(user, body, None).await;
        }
        let mut data = json!({"chat_id":user,"text":clip(body,3900),"link_preview_options":{"is_disabled":true}});
        match button {
            Some("panel") => {
                match self.send_widget("panel", json!({
                    "chat_id":user,"html":PANEL_HTML,"caption":clip(body,900),"allow_queries":true
                })).await? {
                    WidgetDelivery::Sent => return Ok(None),
                    WidgetDelivery::Retry(delay) => return Ok(Some(delay)),
                    WidgetDelivery::Rejected => {
                        data["text"] = json!("Use /tasks to list or switch tasks. Open Settings & forms to manage requests, schedules, and your account.");
                        data["reply_markup"] = self.settings_keyboard();
                    }
                }
            }
            Some("mini") => data["reply_markup"] = self.settings_keyboard(),
            Some(button) if button.starts_with("form/") => {
                let id = uuid::Uuid::parse_str(&button["form/".len()..])?;
                data["reply_markup"] = json!({"inline_keyboard":[[{
                    "text":"Open form","web_app":{"url":format!("{}/mini?form={id}",self.origin)}
                }]]});
            }
            None => {}
            _ => anyhow::bail!("unknown channel button"),
        }
        self.send_message(data).await
    }

    async fn send_suggested_replies(
        &self,
        user: &str,
        body: &str,
        replies: &[store::SuggestedReply],
    ) -> Result<Option<i64>> {
        let user = user.parse::<i64>().context("invalid Telegram recipient")?;
        self.send_reply(user, body, Some(suggested_keyboard(replies)))
            .await
    }

    async fn answer_callback(&self, query: &str, text: &str) -> Result<()> {
        let feedback = self
            .feedback(
                "answerCallbackQuery",
                json!({"callback_query_id":query,"text":text}),
            )
            .await?;
        ensure!(
            feedback.is_none(),
            "Telegram rate-limited the callback answer"
        );
        Ok(())
    }

    async fn send_card(
        &self,
        user: &str,
        card: &Card,
        replies: &[store::SuggestedReply],
    ) -> Result<Option<i64>> {
        let user = user.parse::<i64>().context("invalid Telegram recipient")?;
        let mut data = json!({"chat_id":user,"html":card.html()});
        let mut fallback = json!({"chat_id":user,"text":card.plain_text(),
            "link_preview_options":{"is_disabled":true}});
        let title_length = card.title_utf16_len();
        if title_length > 0 {
            fallback["entities"] = json!([{"type":"bold","offset":0,"length":title_length}]);
        }
        if !replies.is_empty() {
            let keyboard = suggested_keyboard(replies);
            data["reply_markup"] = keyboard.clone();
            fallback["reply_markup"] = keyboard;
        }
        match self.send_widget("card", data).await? {
            WidgetDelivery::Sent => Ok(None),
            WidgetDelivery::Retry(delay) => Ok(Some(delay)),
            WidgetDelivery::Rejected => self.send_message(fallback).await,
        }
    }

    async fn send_choice(&self, user: &str, choice: &WidgetChoice) -> Result<Option<i64>> {
        let user_id = user.parse::<i64>().context("invalid Telegram recipient")?;
        let data = json!({
            "chat_id":user_id,"html":CHOICE_HTML,"caption":choice.label,
            "chat_data":serde_json::to_string(choice)?,"allow_queries":true
        });
        match self.send_widget("choice", data).await? {
            WidgetDelivery::Sent => Ok(None),
            WidgetDelivery::Retry(delay) => Ok(Some(delay)),
            WidgetDelivery::Rejected => {
                let buttons: Vec<_> = choice.options.iter().enumerate().map(|(index, label)| {
                    json!([{"text":label,"callback_data":format!("choice/{}/{index}",choice.id)}])
                }).collect();
                self.send_message(json!({
                    "chat_id":user_id,"text":choice.label,"reply_markup":{"inline_keyboard":buttons}
                }))
                .await
            }
        }
    }

    async fn answer_widget(&self, query: &str, data: &str) -> Result<()> {
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            self.call(
                "answerWidgetQuery",
                json!({"widget_query_id":query,"data":data}),
            ),
        )
        .await??;
        ensure!(
            response["ok"] == true,
            "Telegram rejected the widget answer"
        );
        Ok(())
    }
}

impl Telegram {
    fn settings_keyboard(&self) -> Value {
        json!({"inline_keyboard":[[{
            "text":"Settings & forms","web_app":{"url":format!("{}/mini",self.origin)}
        }]]})
    }
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
                    } => {
                        if let Some(message_id) = telegram_message_id {
                            store::ingest_telegram(
                                db,
                                &operator,
                                external_id,
                                &user,
                                &text,
                                message_id,
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
                    Ok(None)=>Dispatch::Sent,
                    Ok(Some(delay))=>Dispatch::Retry(now()+delay),
                    Err(_)=>Dispatch::Failed
                }
            };
            let (state,retry)=match dispatch {Dispatch::Sent=>("sent",0), Dispatch::Retry(retry)=>("queued",retry), Dispatch::Failed=>("failed",0)};
            sqlx::query("UPDATE outbox SET state=?,retry_at=?,body=CASE WHEN ?='queued' THEN body ELSE '' END,
                button=CASE WHEN ?='queued' THEN button ELSE NULL END,
                widget=CASE WHEN ?='queued' THEN widget ELSE NULL END WHERE id=?")
                .bind(state).bind(retry).bind(state).bind(state).bind(state).bind(item.id).execute(&db).await?;
            Ok::<_,anyhow::Error>(true)
        }.await;
        if !matches!(result, Ok(true)) {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

async fn send_outbound(
    db: &SqlitePool,
    adapter: &dyn ChannelAdapter,
    item: &store::Outbound,
) -> Result<Option<i64>> {
    if item.reaction_event.is_some() {
        return match store::reaction_target(db, item).await? {
            Some(message_id) => adapter.react(&item.user, message_id, &item.body).await,
            None => Ok(None),
        };
    }
    let button = item.button.as_deref();
    if let Some(request) = button.and_then(|button| button.strip_prefix("choice/")) {
        let choice = forms::widget_choice(db, &item.user, request).await?;
        return adapter.send_choice(&item.user, &choice).await;
    }
    let Ok(replies) = store::suggested_replies(db, item.id).await else {
        return Ok(Some(2));
    };
    if let Some(widget) = &item.widget {
        let card: Card = serde_json::from_str(widget)?;
        adapter.send_card(&item.user, &card, &replies).await
    } else if replies.is_empty() {
        adapter.send(&item.user, &item.body, button).await
    } else {
        adapter
            .send_suggested_replies(&item.user, &item.body, &replies)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::{Path, State},
        routing::post,
    };
    use std::{collections::VecDeque, sync::Arc};
    use tokio::{net::TcpListener, sync::Mutex};

    type Mock = Arc<Mutex<(Vec<(String, Value)>, VecDeque<Value>)>>;

    async fn record(
        State(mock): State<Mock>,
        Path(method): Path<String>,
        Json(data): Json<Value>,
    ) -> Json<Value> {
        let mut mock = mock.lock().await;
        mock.0.push((method, data));
        Json(mock.1.pop_front().expect("unexpected Telegram request"))
    }

    #[tokio::test]
    async fn reactions_use_the_latest_private_telegram_message_id() {
        let updates = json!({"ok":true,"result":[
            {"update_id":800,"message":{"message_id":51,"chat":{"id":42,"type":"private"},
                "from":{"id":42,"is_bot":false},"text":"Start a task"}},
            {"update_id":801,"message":{"message_id":52,"chat":{"id":-42,"type":"group"},
                "from":{"id":42,"is_bot":false},"text":"Ignore group messages"}}
        ]});
        let callback = json!({"ok":false,"error_code":429,"parameters":{"retry_after":3}});
        let mock: Mock = Arc::new(Mutex::new((
            Vec::new(),
            VecDeque::from([updates, callback, json!({"ok":true,"result":true})]),
        )));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let telegram = Telegram {
            client: reqwest::Client::new(),
            api: format!("http://{}/botTEST", listener.local_addr().unwrap()),
            origin: "https://example.test".into(),
        };
        let app = Router::new()
            .route("/botTEST/{method}", post(record))
            .with_state(mock.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (updates, offset) = telegram.receive(0).await.unwrap();
        assert_eq!(offset, 802);
        let [
            Inbound::Message {
                external_id,
                user,
                text,
                telegram_message_id: Some(message_id),
            },
        ] = updates.as_slice()
        else {
            panic!("expected one private Telegram message")
        };
        assert_eq!((*external_id, *message_id), (800, 51));

        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&db).await.unwrap();
        store::seed(&db, user).await.unwrap();
        sqlx::query("UPDATE users SET provider='codex' WHERE id=?")
            .bind(user)
            .execute(&db)
            .await
            .unwrap();
        store::ingest_telegram(&db, user, *external_id, user, text, *message_id)
            .await
            .unwrap();
        let task = store::claim(&db).await.unwrap().unwrap();

        assert!(store::REACTION_EMOJIS.contains(&"👍"));
        assert!(store::queue_reaction(&db, &task, "✅").await.is_err());
        let mut other = task.clone();
        other.user = "43".into();
        assert!(!store::queue_reaction(&db, &other, "👀").await.unwrap());
        assert!(store::queue_reaction(&db, &task, "❤️").await.unwrap());
        assert!(!store::queue_reaction(&db, &task, "🔥").await.unwrap());
        let item = store::claim_outbox(&db).await.unwrap().unwrap();
        assert_eq!(send_outbound(&db, &telegram, &item).await.unwrap(), Some(3));
        store::recover(&db).await.unwrap();
        let recovered = store::claim_outbox(&db).await.unwrap().unwrap();
        assert_eq!(recovered.id, item.id);
        let task = store::claim(&db).await.unwrap().unwrap();
        assert_eq!(
            send_outbound(&db, &telegram, &recovered).await.unwrap(),
            None
        );
        sqlx::query("DELETE FROM outbox WHERE id=?")
            .bind(item.id)
            .execute(&db)
            .await
            .unwrap();
        assert!(!store::queue_reaction(&db, &task, "🔥").await.unwrap());

        store::ingest(&db, user, "mock", 1, user, "Synthetic follow-up")
            .await
            .unwrap();
        assert!(!store::queue_reaction(&db, &task, "👍").await.unwrap());
        assert_eq!(store::reaction_target(&db, &item).await.unwrap(), None);
        store::settle(&db, &task, "idle", None).await.unwrap();
        let next = store::claim(&db).await.unwrap().unwrap();
        assert!(!store::queue_reaction(&db, &next, "👍").await.unwrap());
        assert!(store::claim_outbox(&db).await.unwrap().is_none());

        let mock = mock.lock().await;
        let calls = &mock.0;
        let reaction =
            json!({"chat_id":42,"message_id":51,"reaction":[{"type":"emoji","emoji":"❤"}]});
        assert_eq!(calls[1], ("setMessageReaction".into(), reaction));
        assert_eq!(calls[1], calls[2]);
        server.abort();
    }

    #[tokio::test]
    async fn drafts_use_native_entities_without_a_source_keyboard() {
        let mock: Mock = Arc::new(Mutex::new((
            Vec::new(),
            VecDeque::from([
                json!({"ok":false,"error_code":400,"description":"Bad Request: can't parse entities"}),
                json!({"ok":true}),
                json!({"ok":false,"error_code":429,"parameters":{"retry_after":4}}),
                json!({"ok":false,"error_code":404,"description":"Not Found"}),
            ]),
        )));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}/botTEST", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/botTEST/{method}", post(record))
            .with_state(mock.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let telegram = Telegram {
            client: reqwest::Client::new(),
            api,
            origin: "https://example.test".into(),
        };
        let body = "**Gevonden**. Source: https://example.test/docs";
        assert_eq!(telegram.draft("42", 5, body).await.unwrap(), None);
        assert_eq!(
            telegram.draft("42", 5, body).await.unwrap(),
            Some(Duration::from_secs(4))
        );
        assert!(telegram.draft("42", 5, body).await.is_err());

        let mock = mock.lock().await;
        assert_eq!(mock.0.len(), 4);
        for (method, payload) in &mock.0 {
            assert_eq!(method, "sendMessageDraft");
            assert_eq!(payload["draft_id"], 5);
            assert_eq!(
                payload["text"],
                "Gevonden.\n\nSource: https://example.test/docs"
            );
            assert!(payload.get("reply_markup").is_none());
        }
        assert_eq!(
            mock.0[0].1["entities"],
            json!([{"type":"bold","offset":0,"length":8}])
        );
        assert!(mock.0[1].1.get("entities").is_none());
        server.abort();
    }

    #[tokio::test]
    async fn sends_formatted_replies_and_keeps_original_reply_buttons_on_rejection() {
        let accepted = json!({"ok":true});
        let mock: Mock = Arc::new(Mutex::new((
            Vec::new(),
            VecDeque::from([
                accepted.clone(),
                json!({"ok":false,"error_code":400,"description":"Bad Request: can't parse entities"}),
                accepted.clone(),
                json!({"ok":false,"error_code":400,"description":"Bad Request: BUTTON_URL_INVALID"}),
                accepted,
                json!({"ok":false,"error_code":429,"parameters":{"retry_after":4}}),
                json!({"ok":false,"error_code":403,"description":"Forbidden"}),
            ]),
        )));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}/botTEST", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/botTEST/{method}", post(record))
            .with_state(mock.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let telegram = Telegram {
            client: reqwest::Client::new(),
            api,
            origin: "https://example.test".into(),
        };
        let body = "😀 **Gevonden**. Bronnen: [Documentatie](https://example.test/docs)";
        let replies = [store::SuggestedReply {
            token: "12345678-1234-4234-8234-123456789abc".into(),
            body: "Meer uitleg".into(),
        }];

        assert_eq!(telegram.send("42", body, None).await.unwrap(), None);
        for _ in 0..2 {
            assert_eq!(
                telegram
                    .send_suggested_replies("42", body, &replies)
                    .await
                    .unwrap(),
                None
            );
        }
        assert_eq!(telegram.send("42", body, None).await.unwrap(), Some(4));
        assert!(telegram.send("42", body, None).await.is_err());

        let mock = mock.lock().await;
        let calls = &mock.0;
        assert_eq!(calls.len(), 7);
        assert!(calls.iter().all(|(method, _)| method == "sendMessage"));
        let first = &calls[0].1;
        assert_eq!(first["text"], "😀 Gevonden.\n\nBronnen: Documentatie");
        assert_eq!(
            first["entities"][0],
            json!({"type":"bold","offset":3,"length":8})
        );
        let source = json!({"text":"Documentatie","url":"https://example.test/docs"});
        assert_eq!(first["reply_markup"]["inline_keyboard"], json!([[source]]));
        let with_suggestions = &calls[1].1;
        let keyboard = &with_suggestions["reply_markup"]["inline_keyboard"];
        assert_eq!(
            keyboard[0][0]["callback_data"],
            format!("reply/{}", replies[0].token)
        );
        assert_eq!(keyboard[1][0], source);
        assert_eq!(calls[2].1["reply_markup"], with_suggestions["reply_markup"]);
        assert!(calls[2].1.get("entities").is_none());
        assert_eq!(calls[4].1["entities"], with_suggestions["entities"]);
        assert_eq!(calls[4].1["reply_markup"], suggested_keyboard(&replies));
        server.abort();
    }

    #[tokio::test]
    async fn sends_native_cards_and_direct_choices_with_honest_delivery_fallbacks() {
        let accepted = json!({"ok":true,"result":{"widget":{}}});
        let rejected = json!({"ok":false,"error_code":400,"description":"Bad Request: INPUT_MEDIA_WIDGET_NOT_ALLOWED"});
        assert_eq!(widget_rejection(&rejected), "widget_access_restricted");
        let rate_limit = json!({"ok":false,"error_code":429,"parameters":{"retry_after":12}});
        let mock: Mock = Arc::new(Mutex::new((
            Vec::new(),
            VecDeque::from([
                accepted.clone(),
                rate_limit,
                rejected.clone(),
                json!({"ok":true}),
                accepted,
                rejected.clone(),
                json!({"ok":true}),
                rejected,
                json!({"ok":true}),
                json!({"ok":true}),
                json!({"ok":true}),
            ]),
        )));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}/botTEST", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/botTEST/{method}", post(record))
            .with_state(mock.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let telegram = Telegram {
            client: reqwest::Client::new(),
            api,
            origin: "https://example.test".into(),
        };

        let card = Card::new("Hi 👋", "</p><script>&", &["A short item".into()]).unwrap();
        let replies = [store::SuggestedReply {
            token: "12345678-1234-4234-8234-123456789abc".into(),
            body: "Say it in Dutch".into(),
        }];
        assert_eq!(telegram.send_card("42", &card, &[]).await.unwrap(), None);
        assert_eq!(
            telegram.send_card("42", &card, &[]).await.unwrap(),
            Some(12)
        );
        assert_eq!(
            telegram.send_card("42", &card, &replies).await.unwrap(),
            None
        );
        let choice = WidgetChoice {
            id: "12345678-1234-4234-8234-123456789abc".into(),
            label: "Pick a topic".into(),
            options: vec!["Travel".into(), "Cooking".into()],
        };
        assert_eq!(telegram.send_choice("42", &choice).await.unwrap(), None);
        assert_eq!(telegram.send_choice("42", &choice).await.unwrap(), None);
        assert_eq!(
            telegram
                .send("42", "Manage TinyBot", Some("panel"))
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            telegram
                .send("42", "Open settings", Some("mini"))
                .await
                .unwrap(),
            None
        );
        let button = format!("form/{}", choice.id);
        assert_eq!(
            telegram
                .send("42", "I prepared a form for you.", Some(&button))
                .await
                .unwrap(),
            None
        );

        let mock = mock.lock().await;
        let calls = &mock.0;
        assert_eq!(
            calls
                .iter()
                .map(|(method, _)| method.as_str())
                .collect::<Vec<_>>(),
            [
                "sendWidget",
                "sendWidget",
                "sendWidget",
                "sendMessage",
                "sendWidget",
                "sendWidget",
                "sendMessage",
                "sendWidget",
                "sendMessage",
                "sendMessage",
                "sendMessage"
            ]
        );
        let html = calls[0].1["html"].as_str().unwrap();
        assert!(!html.contains("<script>"));
        assert!(html.contains("<h4>Hi 👋</h4>"));
        assert!(html.contains("&lt;/p&gt;&lt;script&gt;&amp;"));
        assert!(html.contains("<li>A short item</li>"));
        assert!(calls[0].1.get("reply_markup").is_none());
        assert_eq!(
            calls[3].1["text"],
            "Hi 👋\n\n</p><script>&\n\n• A short item"
        );
        assert_eq!(
            calls[3].1["entities"],
            json!([{"type":"bold","offset":0,"length":5}])
        );
        assert_eq!(calls[2].1["reply_markup"], calls[3].1["reply_markup"]);
        assert_eq!(
            calls[3].1["reply_markup"]["inline_keyboard"][0][0]["text"],
            "Say it in Dutch"
        );
        let state: Value = serde_json::from_str(calls[4].1["chat_data"].as_str().unwrap()).unwrap();
        assert_eq!(state["options"], json!(["Travel", "Cooking"]));
        assert_eq!(calls[4].1["allow_queries"], true);
        assert_eq!(
            calls[6].1["reply_markup"]["inline_keyboard"][1][0]["callback_data"],
            format!("choice/{}/1", choice.id)
        );
        assert_eq!(
            calls[8].1["reply_markup"]["inline_keyboard"][0][0]["web_app"]["url"],
            "https://example.test/mini"
        );
        assert_eq!(
            calls[9].1["reply_markup"]["inline_keyboard"][0][0]["web_app"]["url"],
            "https://example.test/mini"
        );
        assert_eq!(
            calls[10].1["reply_markup"]["inline_keyboard"][0][0],
            json!({"text":"Open form","web_app":{"url":format!("https://example.test/mini?form={}",choice.id)}})
        );
        server.abort();
    }
}
