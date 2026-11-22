mod widget;

use crate::{
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

pub enum Inbound {
    Message {
        external_id: i64,
        user: String,
        text: String,
    },
    Widget(widget::Query),
}

enum Dispatch {
    Sent,
    Retry(i64),
    Failed,
}

#[async_trait]
pub trait ChannelAdapter: Send + Sync {
    fn name(&self) -> &'static str;
    async fn receive(&self, offset: i64) -> Result<(Vec<Inbound>, i64)>;
    async fn send(&self, user: &str, body: &str, button: Option<&str>) -> Result<Option<i64>>;
    async fn answer_widget(&self, _query: &str, _data: &str) -> Result<()> {
        Ok(())
    }
}

pub struct Telegram {
    client: reqwest::Client,
    token: String,
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
            token,
            origin,
        })
    }

    async fn call(&self, method: &str, data: Value) -> Result<Value> {
        let response = self
            .client
            .post(format!(
                "https://api.telegram.org/bot{}/{method}",
                self.token
            ))
            .json(&data)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Telegram request failed"))?;
        Ok(serde_json::from_slice(
            &net::body(response, 2_000_000).await?,
        )?)
    }
}

#[derive(Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Message>,
    widget_query: Option<widget::Query>,
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

    async fn receive(&self, offset: i64) -> Result<(Vec<Inbound>, i64)> {
        let output = self
            .call(
                "getUpdates",
                json!({"offset":offset,"timeout":25,"allowed_updates":["message","widget_query"]}),
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
        if let Some(button) = button {
            ensure!(button == "mini", "unknown channel button");
            let keyboard = json!({"inline_keyboard":[[{
                "text":"Settings & forms","web_app":{"url":format!("{}/mini",self.origin)}
            }]]});
            if body.chars().count() <= 900 {
                let widget = self.call("sendWidget", json!({
                    "chat_id":user,"html":PANEL_HTML,"caption":body,
                    "show_caption_above_media":true,"allow_queries":true,"protect_content":true,"reply_markup":keyboard
                })).await?;
                if widget["ok"] == true {
                    return Ok(None);
                }
                if let Some(delay) = widget["parameters"]["retry_after"].as_i64() {
                    return Ok(Some(delay.clamp(1, 600)));
                }
                // Older Bot API servers can reject sendWidget. Keep the notice accessible.
            }
            data["reply_markup"] = keyboard;
        }
        let output = self.call("sendMessage", data).await?;
        if output["ok"] == true {
            return Ok(None);
        }
        if let Some(delay) = output["parameters"]["retry_after"].as_i64() {
            return Ok(Some(delay.clamp(1, 600)));
        }
        anyhow::bail!("Telegram rejected the reply")
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
                match adapter.send(&item.user,&item.body,item.button.as_deref()).await {
                    Ok(None)=>Dispatch::Sent,
                    Ok(Some(delay))=>Dispatch::Retry(now()+delay),
                    Err(_)=>Dispatch::Failed
                }
            };
            let (state,retry)=match dispatch {Dispatch::Sent=>("sent",0), Dispatch::Retry(retry)=>("queued",retry), Dispatch::Failed=>("failed",0)};
            sqlx::query("UPDATE outbox SET state=?,retry_at=?,body=CASE WHEN ?='queued' THEN body ELSE '' END,
                button=CASE WHEN ?='queued' THEN button ELSE NULL END WHERE id=?")
                .bind(state).bind(retry).bind(state).bind(state).bind(item.id).execute(&db).await?;
            Ok::<_,anyhow::Error>(true)
        }.await;
        if !matches!(result, Ok(true)) {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}
