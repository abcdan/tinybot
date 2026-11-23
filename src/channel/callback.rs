use super::{Chat, User};
use crate::{forms, store};
use anyhow::Result;
use serde::Deserialize;
use sqlx::SqlitePool;

pub(super) const MCP_APPROVAL_PREFIX: &str = "mcp-approval/";

#[derive(Deserialize)]
pub struct Query {
    pub id: String,
    from: User,
    message: Option<Message>,
    data: Option<String>,
}

#[derive(Deserialize)]
struct Message {
    message_id: Option<i64>,
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
    McpApproval(&'a str, store::McpApprovalDecision),
}

fn selection(data: &str) -> Option<Selection<'_>> {
    if data.len() > 64 {
        return None;
    }
    if let Some(action) = data.strip_prefix(MCP_APPROVAL_PREFIX) {
        let (request, decision) = action.split_once('/')?;
        uuid::Uuid::parse_str(request).ok()?;
        let decision = match decision {
            "once" => store::McpApprovalDecision::Once,
            "always" => store::McpApprovalDecision::Always,
            "deny" => store::McpApprovalDecision::Deny,
            _ => return None,
        };
        return Some(Selection::McpApproval(request, decision));
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

pub(super) struct Answer {
    pub text: &'static str,
    pub clear_keyboard: Option<(i64, i64)>,
}

const UNAVAILABLE: Answer = Answer {
    text: "This option is no longer available.",
    clear_keyboard: None,
};

pub(super) async fn answer(db: &SqlitePool, query: &Query) -> Result<Answer> {
    if !query.is_private() {
        return Ok(UNAVAILABLE);
    }
    let user = query.from.id.to_string();
    let accepted = match query.data.as_deref().and_then(selection) {
        Some(Selection::Reply(token)) => store::redeem_reply(db, &user, token).await?,
        Some(Selection::Choice(request, option)) => {
            forms::answer_choice(db, &user, request, option)
                .await
                .is_ok()
        }
        Some(Selection::McpApproval(request, decision)) => {
            let Some(message_id) = query
                .message
                .as_ref()
                .and_then(|message| message.message_id)
                .filter(|id| *id > 0)
            else {
                return Ok(UNAVAILABLE);
            };
            let text = match decision {
                store::McpApprovalDecision::Once => "Approved for this call.",
                store::McpApprovalDecision::Always => "Always approved for this tool.",
                store::McpApprovalDecision::Deny => "MCP call denied.",
            };
            if store::respond_mcp(db, &user, request, decision)
                .await
                .is_err()
            {
                return Ok(UNAVAILABLE);
            }
            return Ok(Answer {
                text,
                clear_keyboard: Some((query.from.id, message_id)),
            });
        }
        None => false,
    };
    Ok(if accepted {
        Answer {
            text: "Reply sent.",
            clear_keyboard: None,
        }
    } else {
        UNAVAILABLE
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
        assert!(matches!(
            selection(&format!("{MCP_APPROVAL_PREFIX}{id}/once")),
            Some(Selection::McpApproval(request, store::McpApprovalDecision::Once)) if request == id
        ));
        assert!(matches!(
            selection(&format!("{MCP_APPROVAL_PREFIX}{id}/always")),
            Some(Selection::McpApproval(request, store::McpApprovalDecision::Always)) if request == id
        ));
        assert!(matches!(
            selection(&format!("{MCP_APPROVAL_PREFIX}{id}/deny")),
            Some(Selection::McpApproval(request, store::McpApprovalDecision::Deny)) if request == id
        ));
        assert!(selection("/cancel").is_none());
        assert!(selection("reply/not-a-token").is_none());
        assert!(selection(&format!("choice/{id}/-1")).is_none());
        assert!(selection(&format!("{MCP_APPROVAL_PREFIX}{id}/inherit")).is_none());
        assert!(selection(&format!("{MCP_APPROVAL_PREFIX}{id}/always/extra")).is_none());
        assert!(selection(&format!("{MCP_APPROVAL_PREFIX}bad-id/always")).is_none());
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
