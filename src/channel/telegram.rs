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

    async fn download_file(&self, file_id: &str, limit: usize) -> Result<Vec<u8>> {
        media::download(self, file_id, limit).await
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
        let user = user.parse::<i64>().context("invalid Telegram recipient")?;
        let mut data = json!({"chat_id":user,"html":card.html()});
        let mut fallback = json!({"chat_id":user,"text":card.plain_text(),
            "link_preview_options":{"is_disabled":true}});
        add_reply_anchor(&mut data, reply_to);
        add_reply_anchor(&mut fallback, reply_to);
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
            WidgetDelivery::Sent(id) => Ok(MessageDelivery::Sent(id)),
            WidgetDelivery::Retry(delay) => Ok(MessageDelivery::Retry(delay)),
            WidgetDelivery::Rejected => self.send_message(fallback).await,
        }
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
