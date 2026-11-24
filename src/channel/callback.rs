use super::{Chat, User};
use crate::{forms, store};
use anyhow::Result;
use serde::Deserialize;
use sqlx::SqlitePool;

pub(super) const MCP_APPROVAL_PREFIX: &str = "mcp-approval/";
pub(super) const MCP_APPROVAL_ONCE_PREFIX: &str = "mcp-approval-once/";
pub(super) const MAIL_READ_PREFIX: &str = "mail-read/";
pub(super) const MAIL_SEND_PREFIX: &str = "mail-send/";
pub(super) const INVITE_ACCEPT_PREFIX: &str = "invite-accept/";

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
    pub(super) fn listen(&self) -> Option<(String, i64, i64)> {
        if !self.is_private() {
            return None;
        }
        let outbox = self
            .data
            .as_deref()?
            .strip_prefix("listen/")?
            .parse::<i64>()
            .ok()?;
        let message = self.message.as_ref()?.message_id?;
        (outbox > 0 && message > 0).then(|| (self.from.id.to_string(), outbox, message))
    }

    pub(super) fn is_private(&self) -> bool {
        self.message
            .as_ref()
            .is_some_and(|message| message.chat.is_private_with(&self.from))
    }
}

enum Selection<'a> {
    RetryTurn(&'a str),
    AcceptInvite(&'a str),
    ContinueTask(&'a str),
    StopTask(&'a str, i64),
    Reply(&'a str),
    Choice(&'a str, usize),
    McpApproval(&'a str, store::McpApprovalDecision),
    MailRead(&'a str),
    MailSend(&'a str, bool),
}

fn selection(data: &str) -> Option<Selection<'_>> {
    if data.len() > 64 {
        return None;
    }
    let once_only = data.starts_with(MCP_APPROVAL_ONCE_PREFIX);
    if let Some(action) = data
        .strip_prefix(MCP_APPROVAL_PREFIX)
        .or_else(|| data.strip_prefix(MCP_APPROVAL_ONCE_PREFIX))
    {
        let (request, decision) = action.split_once('/')?;
        uuid::Uuid::parse_str(request).ok()?;
        let decision = match decision {
            "once" => store::McpApprovalDecision::Once,
            "always" if !once_only => store::McpApprovalDecision::Always,
            "deny" => store::McpApprovalDecision::Deny,
            _ => return None,
        };
        return Some(Selection::McpApproval(request, decision));
    }
    let (action, remainder) = data.split_once('/')?;
    match action {
        "retry-turn" => {
            uuid::Uuid::parse_str(remainder).ok()?;
            Some(Selection::RetryTurn(remainder))
        }
        "invite-accept" => {
            uuid::Uuid::parse_str(remainder).ok()?;
            Some(Selection::AcceptInvite(remainder))
        }
        "tc" => {
            uuid::Uuid::parse_str(remainder).ok()?;
            Some(Selection::ContinueTask(remainder))
        }
        "ts" => {
            let (task, revision) = remainder.split_once('/')?;
            uuid::Uuid::parse_str(task).ok()?;
            Some(Selection::StopTask(task, revision.parse().ok()?))
        }
        "reply" => {
            uuid::Uuid::parse_str(remainder).ok()?;
            Some(Selection::Reply(remainder))
        }
        "choice" => {
            let (request, option) = remainder.split_once('/')?;
            uuid::Uuid::parse_str(request).ok()?;
            Some(Selection::Choice(request, option.parse().ok()?))
        }
        "mail-read" => {
            uuid::Uuid::parse_str(remainder).ok()?;
            Some(Selection::MailRead(remainder))
        }
        "mail-send" => {
            let (request, decision) = remainder.split_once('/')?;
            uuid::Uuid::parse_str(request).ok()?;
            match decision {
                "once" => Some(Selection::MailSend(request, true)),
                "deny" => Some(Selection::MailSend(request, false)),
                _ => None,
            }
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
    let message_id = query
        .message
        .as_ref()
        .and_then(|message| message.message_id)
        .filter(|id| *id > 0);
    let selected = query.data.as_deref().and_then(selection);
    if let Some(Selection::AcceptInvite(invitation)) = selected {
        let Some(message_id) = message_id else {
            return Ok(UNAVAILABLE);
        };
        return Ok(
            if store::invitations::accept(db, &user, invitation).await? {
                Answer {
                    text: "Invitation accepted.",
                    clear_keyboard: Some((query.from.id, message_id)),
                }
            } else {
                UNAVAILABLE
            },
        );
    }
    if !store::invitations::available(db, &user).await? {
        return Ok(UNAVAILABLE);
    }
    let accepted = match selected {
        Some(Selection::RetryTurn(token)) => {
            if !store::task_failures::retry(db, &user, token).await? {
                return Ok(UNAVAILABLE);
            }
            return Ok(Answer {
                text: "Retry queued.",
                clear_keyboard: message_id.map(|id| (query.from.id, id)),
            });
        }
        Some(Selection::AcceptInvite(_)) => unreachable!(),
        Some(Selection::ContinueTask(task)) => {
            if !store::select_task(db, &user, task).await? {
                return Ok(UNAVAILABLE);
            }
            return Ok(Answer {
                text: "Send your next message to continue this task.",
                clear_keyboard: None,
            });
        }
        Some(Selection::StopTask(task, revision)) => {
            if !store::task_views::stop(db, &user, task, revision).await? {
                return Ok(Answer {
                    text: "Task could not be stopped. Use /status to check it.",
                    clear_keyboard: None,
                });
            }
            return Ok(Answer {
                text: "Task stopped.",
                clear_keyboard: message_id.map(|id| (query.from.id, id)),
            });
        }
        Some(Selection::Reply(token)) => store::redeem_reply(db, &user, token).await?,
        Some(Selection::Choice(request, option)) => {
            forms::answer_choice(db, &user, request, option)
                .await
                .is_ok()
        }
        Some(Selection::McpApproval(request, decision)) => {
            let Some(message_id) = message_id else {
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
        Some(Selection::MailRead(message)) => {
            let Some(message_id) = message_id else {
                return Ok(UNAVAILABLE);
            };
            if !store::mail::reveal_mail(db, &user, message, message_id).await? {
                return Ok(UNAVAILABLE);
            }
            return Ok(Answer {
                text: "Email opened.",
                clear_keyboard: Some((query.from.id, message_id)),
            });
        }
        Some(Selection::MailSend(request, approve)) => {
            let Some(message_id) = message_id else {
                return Ok(UNAVAILABLE);
            };
            if store::respond_email(db, &user, request, approve)
                .await
                .is_err()
            {
                return Ok(UNAVAILABLE);
            }
            return Ok(Answer {
                text: if approve {
                    "Approved to send once."
                } else {
                    "Email denied."
                },
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
        assert!(matches!(
            selection(&format!("{MAIL_READ_PREFIX}{id}")),
            Some(Selection::MailRead(message)) if message == id
        ));
        assert!(matches!(
            selection(&format!("{MAIL_SEND_PREFIX}{id}/once")),
            Some(Selection::MailSend(request, true)) if request == id
        ));
        assert!(matches!(
            selection(&format!("{MAIL_SEND_PREFIX}{id}/deny")),
            Some(Selection::MailSend(request, false)) if request == id
        ));
        assert!(selection(&format!("{MAIL_SEND_PREFIX}{id}/always")).is_none());
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
