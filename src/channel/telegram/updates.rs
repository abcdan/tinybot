use super::Telegram;
use crate::{
    channel::{Inbound, attachment, callback, voice, widget},
    security::clip,
    store,
};
use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Message>,
    message_reaction: Option<ReactionUpdate>,
    widget_query: Option<widget::Query>,
    callback_query: Option<callback::Query>,
}
#[derive(Deserialize)]
struct ReactionUpdate {
    chat: Chat,
    message_id: i64,
    user: Option<User>,
    old_reaction: Vec<Reaction>,
    new_reaction: Vec<Reaction>,
}

#[derive(Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Reaction {
    Emoji {
        emoji: String,
    },
    #[serde(other)]
    Other,
}

impl ReactionUpdate {
    fn receive(self, external_id: i64) -> Option<Inbound> {
        let user = self.user?;
        if !self.chat.is_private_with(&user) || self.message_id <= 0 {
            return None;
        }
        let emoji = self
            .new_reaction
            .into_iter()
            .filter(|reaction| !self.old_reaction.contains(reaction))
            .find_map(|reaction| match reaction {
                Reaction::Emoji { emoji } if !emoji.is_empty() && emoji.len() <= 32 => Some(emoji),
                _ => None,
            })?;
        Some(Inbound::Reaction {
            external_id,
            user: user.id.to_string(),
            message_id: self.message_id,
            emoji,
        })
    }
}
#[derive(Deserialize)]
struct Message {
    message_id: i64,
    chat: Chat,
    from: Option<User>,
    text: Option<String>,
    caption: Option<String>,
    #[serde(default)]
    photo: Vec<attachment::Photo>,
    document: Option<attachment::Document>,
    voice: Option<voice::Note>,
    reply_to_message: Option<RepliedMessage>,
    quote: Option<TextQuote>,
}

#[derive(Deserialize)]
struct RepliedMessage {
    chat: Chat,
    message_id: i64,
    from: Option<User>,
    text: Option<String>,
    caption: Option<String>,
}

#[derive(Deserialize)]
struct TextQuote {
    text: String,
}

impl Message {
    fn reply_context(&self) -> Option<store::ReplyContext> {
        let reply = self.reply_to_message.as_ref()?;
        if self.chat.kind != "private"
            || reply.chat.kind != "private"
            || reply.chat.id != self.chat.id
            || reply.message_id <= 0
            || reply.message_id == self.message_id
        {
            return None;
        }
        let excerpt = self
            .quote
            .as_ref()
            .map(|quote| quote.text.as_str())
            .filter(|text| !text.trim().is_empty())
            .or(reply.text.as_deref())
            .or(reply.caption.as_deref())
            .map(|text| clip(text.trim(), 1600).to_owned());
        Some(store::ReplyContext {
            message_id: reply.message_id,
            excerpt,
            from_bot: reply.from.as_ref().is_some_and(|user| user.is_bot),
        })
    }
}
#[derive(Deserialize)]
pub(in crate::channel) struct Chat {
    id: i64,
    #[serde(rename = "type")]
    kind: String,
}
#[derive(Deserialize)]
pub(in crate::channel) struct User {
    pub(in crate::channel) id: i64,
    is_bot: bool,
}

impl Chat {
    pub(in crate::channel) fn is_private_with(&self, user: &User) -> bool {
        self.kind == "private" && self.id == user.id && !user.is_bot
    }
}

impl Telegram {
    pub(super) async fn receive_updates(&self, offset: i64) -> Result<(Vec<Inbound>, i64)> {
        let output = self
            .call(
                "getUpdates",
                json!({"offset":offset,"timeout":25,"allowed_updates":["message","message_reaction","widget_query","callback_query"]}),
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
                if let Some(reaction) = update.message_reaction {
                    return reaction.receive(update.update_id);
                }
                let message = update.message?;
                let user = message.from.as_ref()?;
                if !message.chat.is_private_with(user) {
                    return None;
                }
                let reply = message.reply_context();
                if let Some(note) = message.voice {
                    return Some(Inbound::Voice(voice::Received::new(
                        update.update_id,
                        user.id.to_string(),
                        message.message_id,
                        note,
                        reply,
                    )));
                }
                if let Some(source) = attachment::Source::select(message.photo, message.document) {
                    return Some(Inbound::Attachment(attachment::Received {
                        external_id: update.update_id,
                        user: user.id.to_string(),
                        message_id: message.message_id,
                        input: message.caption.unwrap_or_default(),
                        source,
                        reply,
                    }));
                }
                Some(Inbound::Message {
                    external_id: update.update_id,
                    user: user.id.to_string(),
                    text: message.text.unwrap_or_default(),
                    telegram_message_id: Some(message.message_id),
                    reply,
                })
            })
            .collect();
        Ok((messages, offset))
    }
}
