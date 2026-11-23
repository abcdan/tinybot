use super::{Chat, User};
use crate::{forms, store};
use anyhow::Result;
use serde::Deserialize;
use sqlx::SqlitePool;

#[derive(Deserialize)]
pub struct Query {
    pub id: String,
    from: User,
    message: Option<Message>,
    data: Option<String>,
}

#[derive(Deserialize)]
struct Message {
    chat: Chat,
}

impl Query {
    pub(super) fn is_private(&self) -> bool {
        self.message
            .as_ref()
            .is_some_and(|message| message.chat.is_private_with(&self.from))
    }
}

enum Selection<'a> {
    Reply(&'a str),
    Choice(&'a str, usize),
}

fn selection(data: &str) -> Option<Selection<'_>> {
    if data.len() > 64 {
        return None;
    }
    let (action, remainder) = data.split_once('/')?;
    match action {
        "reply" => {
            uuid::Uuid::parse_str(remainder).ok()?;
            Some(Selection::Reply(remainder))
        }
        "choice" => {
            let (request, option) = remainder.split_once('/')?;
            uuid::Uuid::parse_str(request).ok()?;
            Some(Selection::Choice(request, option.parse().ok()?))
        }
        _ => None,
    }
}

pub(super) async fn answer(db: &SqlitePool, query: &Query) -> Result<&'static str> {
    let user = query.from.id.to_string();
    let accepted = match query.data.as_deref().and_then(selection) {
        Some(Selection::Reply(token)) => store::redeem_reply(db, &user, token).await?,
        Some(Selection::Choice(request, option)) => {
            forms::answer_choice(db, &user, request, option)
                .await
                .is_ok()
        }
        None => false,
    };
    Ok(if accepted {
        "Reply sent."
    } else {
        "This option is no longer available."
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_only_known_callback_routes() {
        let id = "700c37b2-438a-4b2e-92ce-612ed94bc8ad";
        assert!(matches!(
            selection(&format!("reply/{id}")),
            Some(Selection::Reply(token)) if token == id
        ));
        assert!(matches!(
            selection(&format!("choice/{id}/2")),
            Some(Selection::Choice(request, 2)) if request == id
        ));
        assert!(selection("/cancel").is_none());
        assert!(selection("reply/not-a-token").is_none());
        assert!(selection(&format!("choice/{id}/-1")).is_none());
    }

    #[test]
    fn callback_must_come_from_the_senders_private_chat() {
        for (kind, from, chat, is_bot, accepted) in [
            ("private", 42, 42, false, true),
            ("private", 43, 42, false, false),
            ("private", 42, 42, true, false),
            ("group", 42, 42, false, false),
        ] {
            let query: Query = serde_json::from_value(json!({
                "id":"callback",
                "from":{"id":from,"is_bot":is_bot},
                "message":{"message_id":1,"chat":{"id":chat,"type":kind}},
                "data":"reply/token"
            }))
            .unwrap();
            assert_eq!(query.is_private(), accepted);
        }
    }
}
