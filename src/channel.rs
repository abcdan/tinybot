mod callback;
mod card;
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
    },
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
        let output =
            tokio::time::timeout(Duration::from_secs(3), self.call(method, data)).await??;
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
        let response = self.call("sendMessage", data).await?;
        if response["ok"] == true {
            return Ok(None);
        }
        if let Some(delay) = retry_after(&response) {
            return Ok(Some(delay));
        }
        anyhow::bail!("Telegram rejected the reply")
    }
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
    chat: Chat,
    from: Option<User>,
    text: Option<String>,
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
        self.feedback(
            "sendMessageDraft",
            json!({"chat_id":user,"draft_id":id,"text":text}),
        )
        .await
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
                Some(Inbound::Message {
                    external_id: update.update_id,
                    user: user.id.to_string(),
                    text: message.text.unwrap_or_default(),
                })
            })
            .collect();
        Ok((messages, offset))
    }

    async fn send(&self, user: &str, body: &str, button: Option<&str>) -> Result<Option<i64>> {
        let user = user.parse::<i64>().context("invalid Telegram recipient")?;
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
        self.send_message(json!({
            "chat_id":user,"text":clip(body,3900),"link_preview_options":{"is_disabled":true},
            "reply_markup":suggested_keyboard(replies)
        }))
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

pub async fn listen(db: SqlitePool, operator: String, adapter: std::sync::Arc<dyn ChannelAdapter>) {
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
                    } => {
                        store::ingest(&db, &operator, adapter.name(), external_id, &user, &text)
                            .await?;
                    }
                    Inbound::Widget(query) => {
                        let data = widget::answer(&db, &query).await;
                        if adapter.answer_widget(&query.id, &data).await.is_err() {
                            tracing::debug!("widget answer expired or could not be delivered");
                        }
                    }
                    Inbound::Callback(query) => {
                        let text = callback::answer(&db, &query).await?;
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
        server.abort();
    }
}
