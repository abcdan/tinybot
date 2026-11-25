mod attachment;
mod callback;
mod card;
mod dispatch;
mod formatting;
mod groups;
mod media;
mod telegram;
mod voice;
mod voice_request;
mod widget;

pub use card::Card;
pub use dispatch::{listen, outbox};
pub(crate) use formatting::telegram_reply_parts;
pub use telegram::Telegram;
use telegram::{Chat, User};

use crate::{forms::WidgetChoice, store};
use anyhow::Result;
use async_trait::async_trait;
use std::time::Duration;

pub enum Inbound {
    Message {
        external_id: i64,
        user: String,
        text: String,
        telegram_message_id: Option<i64>,
        reply: Option<store::ReplyContext>,
    },
    Reaction {
        external_id: i64,
        user: String,
        message_id: i64,
        emoji: String,
    },
    Attachment(attachment::Received),
    Group(groups::Message),
    Voice(voice::Received),
    Widget(widget::Query),
    Callback(callback::Query),
}

#[derive(Debug, PartialEq, Eq)]
pub enum MessageDelivery {
    Sent(Option<i64>),
    Retry(i64),
    Skipped,
}

#[derive(Debug)]
pub(super) struct DeliveryUncertain;

impl std::fmt::Display for DeliveryUncertain {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Telegram did not confirm the delivery")
    }
}

impl std::error::Error for DeliveryUncertain {}

#[derive(Debug)]
pub(crate) struct UploadUncertain;

impl std::fmt::Display for UploadUncertain {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Telegram did not confirm the file upload")
    }
}

impl std::error::Error for UploadUncertain {}

#[async_trait]
pub trait ChannelAdapter: Send + Sync {
    fn name(&self) -> &'static str;
    async fn receive(&self, offset: i64) -> Result<(Vec<Inbound>, i64)>;
    async fn send(
        &self,
        user: &str,
        body: &str,
        button: Option<&str>,
        reply_to: Option<i64>,
    ) -> Result<MessageDelivery>;

    async fn send_plain(
        &self,
        user: &str,
        body: &str,
        reply_to: Option<i64>,
    ) -> Result<MessageDelivery> {
        self.send(user, body, None, reply_to).await
    }

    async fn bot_identity(&self) -> Result<(i64, String)> {
        anyhow::bail!("channel does not have a Telegram bot identity")
    }

    async fn group_admin(&self, _chat: i64, _user: &str) -> Result<bool> {
        Ok(false)
    }

    async fn send_group(
        &self,
        _route: store::groups::GroupRoute,
        _body: &str,
        _reply: Option<i64>,
    ) -> Result<MessageDelivery> {
        anyhow::bail!("channel does not support group messages")
    }

    async fn download_file(&self, _file_id: &str, _limit: usize) -> Result<Vec<u8>> {
        anyhow::bail!("channel does not support file downloads")
    }

    async fn upload_file(
        &self,
        _user: &str,
        _kind: &str,
        _name: &str,
        _bytes: &[u8],
        _reply_to: Option<i64>,
    ) -> Result<MessageDelivery> {
        anyhow::bail!("channel does not support file uploads")
    }

    async fn answer_widget(&self, _query: &str, _data: &str) -> Result<()> {
        Ok(())
    }

    async fn send_suggested_replies(
        &self,
        user: &str,
        body: &str,
        _replies: &[store::SuggestedReply],
        reply_to: Option<i64>,
    ) -> Result<MessageDelivery> {
        self.send(user, body, None, reply_to).await
    }

    async fn answer_callback(&self, _query: &str, _text: &str) -> Result<()> {
        Ok(())
    }

    async fn clear_inline_keyboard(&self, _chat_id: i64, _message_id: i64) -> Result<()> {
        Ok(())
    }

    async fn send_card(
        &self,
        user: &str,
        card: &Card,
        replies: &[store::SuggestedReply],
        reply_to: Option<i64>,
    ) -> Result<MessageDelivery> {
        self.send_suggested_replies(user, &card.plain_text(), replies, reply_to)
            .await
    }

    async fn send_choice(&self, user: &str, choice: &WidgetChoice) -> Result<MessageDelivery> {
        self.send(user, &choice.label, Some("mini"), None).await
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

#[cfg(test)]
mod tests;
