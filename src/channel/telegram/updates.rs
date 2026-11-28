use super::Telegram;
use crate::{
    channel::{Inbound, attachment, callback, groups, voice, widget},
    security::clip,
    store,
};
use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
struct Update {
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
    message_thread_id: Option<i64>,
    sender_chat: Option<Chat>,
}

#[derive(Deserialize)]
struct RepliedMessage {
    chat: Chat,
    message_id: i64,
    from: Option<User>,
    text: Option<String>,
    caption: Option<String>,
    message_thread_id: Option<i64>,
}

#[derive(Deserialize)]
struct TextQuote {
    text: String,
}

impl Message {
    fn group(self, external: i64) -> Option<Inbound> {
        let user = self.from?;
        if user.is_bot
            || self.sender_chat.is_some()
            || self.message_id <= 0
            || !matches!(self.chat.kind.as_str(), "group" | "supergroup")
            || self.chat.id >= 0
        {
            return None;
        }
        let topic = self.message_thread_id.unwrap_or(0);
        if topic < 0 {
            return None;
        }
        let text = clip(self.text.as_deref()?.trim(), 8000);
        if text.is_empty() {
            return None;
        }
        let reply = self.reply_to_message.and_then(|reply| {
            let author = reply.from?;
            (author.is_bot
                && reply.chat.id == self.chat.id
                && reply.message_thread_id.unwrap_or(0) == topic
                && reply.message_id > 0
                && reply.message_id != self.message_id)
                .then_some((author.id, reply.message_id))
        });
        if reply.is_none() && !text.starts_with("/ask@") && !text.starts_with("/tinybot_enable@") {
            return None;
        }
        Some(Inbound::Group(groups::Message {
            external,
            user: user.id.to_string(),
            route: store::groups::GroupRoute {
                chat: self.chat.id,
                topic,
            },
            message: self.message_id,
            text: text.to_owned(),
            reply,
        }))
    }

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
        let mut output = self
            .call(
                "getUpdates",
                json!({"offset":offset,"timeout":25,"allowed_updates":["message","message_reaction","widget_query","callback_query"]}),
            )
            .await?;
        ensure!(
            output["ok"] == true,
            "Telegram polling failed. Check the bot token and webhook configuration."
        );
        decode_updates(output["result"].take(), offset)
    }
}

fn decode_updates(result: Value, offset: i64) -> Result<(Vec<Inbound>, i64)> {
    let Value::Array(updates) = result else {
        anyhow::bail!("Telegram polling result is missing its update list.");
    };
    let is_empty = updates.is_empty();
    let mut next: Option<i64> = None;
    let messages = updates
        .into_iter()
        .filter_map(|value| {
            let id = value["update_id"].as_i64().filter(|id| *id > 0)?;
            let after = id.checked_add(1)?;
            next = Some(next.map_or(after, |latest| latest.max(after)));
            let Ok(update) = serde_json::from_value::<Update>(value) else {
                tracing::debug!(update_id = id, "Telegram update payload could not be read");
                return None;
            };
            if let Some(query) = update.widget_query {
                return query.is_private().then_some(Inbound::Widget(query));
            }
            if let Some(query) = update.callback_query {
                return query.is_private().then_some(Inbound::Callback(query));
            }
            if let Some(reaction) = update.message_reaction {
                return reaction.receive(id);
            }
            let message = update.message?;
            let user = message.from.as_ref()?;
            if !message.chat.is_private_with(user) {
                return message.group(id);
            }
            let reply = message.reply_context();
            if let Some(note) = message.voice {
                return Some(Inbound::Voice(voice::Received::new(
                    id,
                    user.id.to_string(),
                    message.message_id,
                    note,
                    reply,
                )));
            }
            if let Some(source) = attachment::Source::select(message.photo, message.document) {
                return Some(Inbound::Attachment(attachment::Received {
                    external_id: id,
                    user: user.id.to_string(),
                    message_id: message.message_id,
                    input: message.caption.unwrap_or_default(),
                    source,
                    reply,
                }));
            }
            Some(Inbound::Message {
                external_id: id,
                user: user.id.to_string(),
                text: message.text.unwrap_or_default(),
                telegram_message_id: Some(message.message_id),
                reply,
            })
        })
        .collect();
    ensure!(
        is_empty || next.is_some(),
        "Telegram polling result has no valid update IDs."
    );
    Ok((messages, next.unwrap_or(offset)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_updates_do_not_block_valid_messages_or_trailing_offsets() {
        let message = |id| {
            json!({"update_id":id,"message":{
                "message_id":51,"chat":{"id":42,"type":"private"},
                "from":{"id":42,"is_bot":false},"text":"Hello"
            }})
        };
        let (messages, next) = decode_updates(
            json!([
                message(11),
                {"update_id":12,"message":{"message_id":"invalid"}},
                {"update_id":13,"widget_query":{}},
                {"update_id":15,"message_reaction":{}},
                message(14),
                {"update_id":"invalid","message":{}}
            ]),
            11,
        )
        .unwrap();
        assert_eq!(next, 16);
        let [
            Inbound::Message {
                external_id: 11, ..
            },
            Inbound::Message {
                external_id: 14, ..
            },
        ] = messages.as_slice()
        else {
            panic!("expected both valid messages");
        };
        assert_eq!(decode_updates(json!([]), 16).unwrap().1, 16);
        assert!(decode_updates(json!([{"update_id":i64::MAX}]), 16).is_err());
        assert!(decode_updates(json!([{"update_id":-1}]), 16).is_err());
        assert_eq!(decode_updates(json!([message(5)]), 16).unwrap().1, 6);
        assert!(decode_updates(json!({}), 16).is_err());
    }
}
