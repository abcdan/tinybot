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
