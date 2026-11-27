mod api;
mod rich;
mod updates;

use super::{Card, ChannelAdapter, Inbound, MessageDelivery, callback, formatting, media};
use crate::{forms::WidgetChoice, security::clip, store};
use anyhow::{Context, Result, ensure};
pub(super) use api::suggested_keyboard;
#[cfg(test)]
pub(super) use api::widget_rejection;
use api::{WidgetDelivery, add_reply_anchor};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::time::Duration;
pub(super) use updates::{Chat, User};

const PANEL_HTML: &str = include_str!("../../web/telegram-widget.html");
const CHOICE_HTML: &str = include_str!("../../web/telegram-choice.html");

pub struct Telegram {
    pub(super) client: reqwest::Client,
    pub(super) api: String,
    pub(super) origin: String,
    pub(super) identity: tokio::sync::OnceCell<(i64, String)>,
}

#[cfg(test)]
impl Telegram {
    pub(crate) fn fixture(api: String) -> Self {
        let mut telegram = Self::new("unused".into(), "https://example.test".into()).unwrap();
        telegram.api = api;
        telegram
    }
}

#[async_trait]
impl ChannelAdapter for Telegram {
    fn name(&self) -> &'static str {
        "telegram"
    }

    async fn bot_identity(&self) -> Result<(i64, String)> {
        Ok(self
            .identity
            .get_or_try_init(|| async {
                let response = self.call("getMe", json!({})).await?;
                ensure!(
                    response["ok"] == true,
                    "Telegram bot identity is unavailable"
                );
                let id = response["result"]["id"]
                    .as_i64()
                    .filter(|id| *id > 0)
                    .context("missing bot ID")?;
                let name = response["result"]["username"]
                    .as_str()
                    .filter(|name| {
                        !name.is_empty()
                            && name.len() <= 64
                            && name
                                .bytes()
                                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                    })
                    .context("missing bot name")?;
                Ok::<_, anyhow::Error>((id, name.to_owned()))
            })
            .await?
            .clone())
    }

    async fn group_admin(&self, chat: i64, user: &str) -> Result<bool> {
        let user = user.parse::<i64>()?;
        let response = self
            .call("getChatAdministrators", json!({"chat_id":chat}))
            .await?;
        Ok(response["ok"] == true
            && response["result"].as_array().is_some_and(|members| {
                members.iter().any(|member| {
                    member["user"]["id"].as_i64() == Some(user)
                        && matches!(member["status"].as_str(), Some("creator" | "administrator"))
                })
            }))
    }

    async fn send_group(
        &self,
        route: store::groups::GroupRoute,
        body: &str,
        reply_to: Option<i64>,
    ) -> Result<MessageDelivery> {
        let rendered = formatting::render(body);
        let mut data = json!({"chat_id":route.chat,"text":rendered.text,
            "link_preview_options":{"is_disabled":true}});
        if route.topic > 0 {
            data["message_thread_id"] = json!(route.topic);
        }
        add_reply_anchor(&mut data, reply_to);
        if !rendered.entities.is_empty() {
            data["entities"] = json!(rendered.entities);
        }
        self.send_message(data).await
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

    async fn download_file(&self, file_id: &str, limit: usize) -> Result<Vec<u8>> {
        media::download(self, file_id, limit).await
    }

    async fn upload_file(
        &self,
        user: &str,
        kind: &str,
        name: &str,
        bytes: &[u8],
        reply_to: Option<i64>,
    ) -> Result<MessageDelivery> {
        self.upload(user, kind, name, bytes, reply_to).await
    }

    async fn receive(&self, offset: i64) -> Result<(Vec<Inbound>, i64)> {
        self.receive_updates(offset).await
    }

    async fn send(
        &self,
        user: &str,
        body: &str,
        button: Option<&str>,
        reply_to: Option<i64>,
    ) -> Result<MessageDelivery> {
        let user = user.parse::<i64>().context("invalid Telegram recipient")?;
        if button.is_none() {
            return self.send_reply(user, body, None, reply_to).await;
        }
        let mut data = json!({"chat_id":user,"text":clip(body,3900),"link_preview_options":{"is_disabled":true}});
        add_reply_anchor(&mut data, reply_to);
        match button {
            Some("panel") => {
                match self.send_widget("panel", json!({
                    "chat_id":user,"html":PANEL_HTML,"caption":clip(body,900),"allow_queries":true
                })).await? {
                    WidgetDelivery::Sent(id) => return Ok(MessageDelivery::Sent(id)),
                    WidgetDelivery::Retry(delay) => return Ok(MessageDelivery::Retry(delay)),
                    WidgetDelivery::Rejected => {
                        data["text"] = json!("Use /tasks to list or switch tasks. Open Settings & forms to manage requests, schedules, and your account.");
                        data["reply_markup"] = self.settings_keyboard();
                    }
                }
            }
            Some("mini") => data["reply_markup"] = self.settings_keyboard(),
            Some(button) if button.starts_with(store::task_failures::RETRY) => {
                uuid::Uuid::parse_str(&button[store::task_failures::RETRY.len()..])?;
                data["reply_markup"] = json!({"inline_keyboard":[[
                    {"text":"Retry","callback_data":button},
                    {"text":"Settings & forms","web_app":{"url":format!("{}/mini",self.origin)}}
                ]]});
            }
            Some(button) if button.starts_with("mail-setup/")=>{
                let request=uuid::Uuid::parse_str(&button["mail-setup/".len()..])?;
                data["reply_markup"]=json!({"inline_keyboard":[[{"text":"Connect email","web_app":{"url":format!("{}/mini?connect=mail&request={request}",self.origin)}}]]});
            }
            Some("docblock") => data["reply_markup"] = json!({"inline_keyboard":[[{
                "text":"Connect Docblock","web_app":{"url":format!("{}/mini?connect=docblock",self.origin)}
            }]]}),
            Some(button) if button.starts_with(callback::MCP_APPROVAL_ONCE_PREFIX) => {
                let prefix = callback::MCP_APPROVAL_ONCE_PREFIX;
                let id = uuid::Uuid::parse_str(&button[prefix.len()..])?;
                data["reply_markup"] = json!({"inline_keyboard":[[
                    {"text":"Approve once","callback_data":format!("{prefix}{id}/once")},
                    {"text":"Deny","callback_data":format!("{prefix}{id}/deny")}
                ], [{"text":"Review in Settings","web_app":{"url":format!("{}/mini",self.origin)}}]]});
            }
            Some(button) if button.starts_with(callback::INVITE_ACCEPT_PREFIX) => {
                callback::invite_consent(&button[callback::INVITE_ACCEPT_PREFIX.len()..])
                    .context("invalid invite callback")?;
                data["reply_markup"] = json!({"inline_keyboard":[[{
                    "text":"Accept invitation","callback_data":button
                }]]});
            }
            Some(button) if button.starts_with("task/") || button.starts_with("tq/") => {
                let mut fields = button.split('/').skip(1);
                let task = fields.next().context("missing task")?;
                let revision = fields.next().context("missing task revision")?;
                let state = fields.next().context("missing task state")?;
                let mut buttons = Vec::new();
                if !matches!(state, "cancelled" | "revoked") {
                    let label = if state == "completed" { "Reopen" } else { "Continue" };
                    buttons.push(json!({"text":label,"callback_data":format!("tc/{task}/{revision}")}));
                }
                if matches!(state, "queued" | "running" | "waiting" | "setup") {
                    buttons.push(json!({"text":"Stop","callback_data":format!("ts/{task}/{revision}")}));
                }
                if store::task_views::completable(state) {
                    buttons.push(json!({"text":"Done","callback_data":format!("td/{task}/{revision}")}));
                }
                let mut rows = Vec::new();
                if !buttons.is_empty() {
                    rows.push(buttons);
                }
                if let Ok(task) = uuid::Uuid::parse_str(task) {
                    let label = if matches!(state, "waiting" | "setup") { "Review" } else { "Open task" };
                    rows.push(vec![json!({"text":label,"web_app":{"url":format!("{}/mini?task={task}",self.origin)}})]);
                }
                if !rows.is_empty() {
                    data["reply_markup"] = json!({"inline_keyboard":rows});
                }
            }
            Some(button) if button.starts_with(callback::MCP_APPROVAL_PREFIX) => {
                let id = uuid::Uuid::parse_str(&button[callback::MCP_APPROVAL_PREFIX.len()..])?;
                let prefix = callback::MCP_APPROVAL_PREFIX;
                data["reply_markup"] = json!({"inline_keyboard":[
                    [
                        {"text":"Approve once","callback_data":format!("{prefix}{id}/once")},
                        {"text":"Always approve","callback_data":format!("{prefix}{id}/always")}
                    ],
                    [{"text":"Deny","callback_data":format!("{prefix}{id}/deny")}]
                ]});
            }
            Some(button) if button.starts_with(callback::MAIL_READ_PREFIX) => {
                let id = uuid::Uuid::parse_str(&button[callback::MAIL_READ_PREFIX.len()..])?;
                data["reply_markup"] = json!({"inline_keyboard":[[{
                    "text":"Read more","callback_data":format!("{}{id}", callback::MAIL_READ_PREFIX)
                }]]});
            }
            Some(button) if button.starts_with(callback::MAIL_SEND_PREFIX) => {
                let id = uuid::Uuid::parse_str(&button[callback::MAIL_SEND_PREFIX.len()..])?;
                let prefix = callback::MAIL_SEND_PREFIX;
                data["reply_markup"] = json!({"inline_keyboard":[[
                    {"text":"Approve once","callback_data":format!("{prefix}{id}/once")},
                    {"text":"Deny","callback_data":format!("{prefix}{id}/deny")}
                ]]});
            }
            Some(button) if button.starts_with("form/") || button.starts_with(store::settings_requests::BUTTON_PREFIX) => {
                let request = button.strip_prefix("form/")
                    .or_else(|| button.strip_prefix(store::settings_requests::BUTTON_PREFIX))
                    .context("invalid form destination")?;
                let id = uuid::Uuid::parse_str(request)?;
                data["reply_markup"] = json!({"inline_keyboard":[[{
                    "text":"Open form","web_app":{"url":format!("{}/mini?form={id}",self.origin)}
                }]]});
            }
            None => {}
            _ => anyhow::bail!("unknown channel button"),
        }
        self.send_message(data).await
    }

    async fn send_plain(
        &self,
        user: &str,
        body: &str,
        reply_to: Option<i64>,
    ) -> Result<MessageDelivery> {
        let user = user.parse::<i64>().context("invalid Telegram recipient")?;
        let mut data = json!({"chat_id":user,"text":clip(body,3900),"link_preview_options":{"is_disabled":true}});
        add_reply_anchor(&mut data, reply_to);
        self.send_message(data).await
    }

    async fn send_suggested_replies(
        &self,
        user: &str,
        body: &str,
        replies: &[store::SuggestedReply],
        reply_to: Option<i64>,
    ) -> Result<MessageDelivery> {
        let user = user.parse::<i64>().context("invalid Telegram recipient")?;
        self.send_reply(user, body, Some(suggested_keyboard(replies)), reply_to)
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

    async fn clear_inline_keyboard(&self, chat_id: i64, message_id: i64) -> Result<()> {
        let feedback = self
            .feedback(
                "editMessageReplyMarkup",
                json!({
                    "chat_id":chat_id,
                    "message_id":message_id,
                    "reply_markup":{"inline_keyboard":[]}
                }),
            )
            .await?;
        ensure!(
            feedback.is_none(),
            "Telegram rate-limited the keyboard update"
        );
        Ok(())
    }

    async fn send_card(
        &self,
        user: &str,
        card: &Card,
        replies: &[store::SuggestedReply],
        reply_to: Option<i64>,
    ) -> Result<MessageDelivery> {
        self.send_task_card(
            user.parse()?,
            card,
            (!replies.is_empty()).then(|| suggested_keyboard(replies)),
            reply_to,
        )
        .await
    }

    async fn send_choice(&self, user: &str, choice: &WidgetChoice) -> Result<MessageDelivery> {
        let user_id = user.parse::<i64>().context("invalid Telegram recipient")?;
        let data = json!({
            "chat_id":user_id,"html":CHOICE_HTML,"caption":choice.label,
            "chat_data":serde_json::to_string(choice)?,"allow_queries":true
        });
        match self.send_widget("choice", data).await? {
            WidgetDelivery::Sent(id) => Ok(MessageDelivery::Sent(id)),
            WidgetDelivery::Retry(delay) => Ok(MessageDelivery::Retry(delay)),
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
