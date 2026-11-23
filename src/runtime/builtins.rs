use super::{Intent, Outcome, Risk, Tool, ToolContext, effects, string};
use crate::{mcp, net, schedules, security::now};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};

#[derive(Clone, Copy)]
pub(super) enum Builtin {
    Remember,
    RecallMemory,
    SetGoal,
    Delegate,
    Discover,
    Connect,
    ListMcp,
    CallMcp,
    Browse,
    SearchWeb,
    Form,
    Input,
    AgentSpec,
    Schedule,
    ListSchedules,
    CancelSchedule,
    Files,
    Python,
    SendReply,
    SendCard,
    React,
}

pub fn tools() -> Vec<Box<dyn Tool>> {
    [
        Builtin::Remember,
        Builtin::RecallMemory,
        Builtin::SetGoal,
        Builtin::Delegate,
        Builtin::Discover,
        Builtin::Connect,
        Builtin::ListMcp,
        Builtin::CallMcp,
        Builtin::Browse,
        Builtin::SearchWeb,
        Builtin::Form,
        Builtin::Input,
        Builtin::AgentSpec,
        Builtin::Schedule,
        Builtin::ListSchedules,
        Builtin::CancelSchedule,
        Builtin::Files,
        Builtin::Python,
        Builtin::SendReply,
        Builtin::SendCard,
        Builtin::React,
    ]
    .into_iter()
    .map(|tool| Box::new(tool) as Box<dyn Tool>)
    .collect()
}

#[async_trait]
impl Tool for Builtin {
    fn name(&self) -> &'static str {
        match self {
            Self::Remember => "remember",
            Self::RecallMemory => "recall_memory",
            Self::SetGoal => "set_goal",
            Self::Delegate => "delegate",
            Self::Discover => "discover_mcp",
            Self::Connect => "connect_mcp",
            Self::ListMcp => "list_mcp_tools",
            Self::CallMcp => "call_mcp",
            Self::Browse => "browse",
            Self::SearchWeb => "search_web",
            Self::Form => "request_form",
            Self::Input => "request_input",
            Self::AgentSpec => "propose_agent",
            Self::Schedule => "schedule",
            Self::ListSchedules => "list_schedules",
            Self::CancelSchedule => "cancel_schedule",
            Self::Files => "task_files",
            Self::Python => "python",
            Self::SendReply => "send_reply",
            Self::SendCard => "send_card",
            Self::React => "react_to_message",
        }
    }

    fn describe(&self) -> &'static str {
        match self {
            Self::Remember => {
                "Save a durable user fact or preference. The default scope is the current agent's private memory. The Chief can choose group for all of the user's agents, or pass an owned agent ID for that helper's private memory."
            }
            Self::RecallMemory => {
                "Search durable memory with a short keyword or phrase. Includes group notes where authorized. The Chief can supply an owned agent ID to search that agent's private notes."
            }
            Self::SetGoal => "Record the current task goal. Args: text.",
            Self::Delegate => {
                "Ask another agent to carry out a self-contained brief and return findings to you. Choose a matching saved agent by ID when available. Include a handoff: one short first-person message in the user's language saying what you're asking the helper and that you'll follow up. Include the helper's name if you selected one."
            }
            Self::Discover => "Find official remote MCP endpoints. Args: name.",
            Self::Connect => {
                "Prepare a discovered connector after review. Args: name, endpoint, transport (auto, streamable-http, sse or stdio)."
            }
            Self::ListMcp => {
                "Get available MCP tool names and schemas. Args: connector, optional search."
            }
            Self::CallMcp => "Call a connector tool. Args: connector, tool, arguments object.",
            Self::Browse => {
                "Read visible text and links from a public website. Args: url (an HTTPS URL or a website domain)."
            }
            Self::SearchWeb => {
                "Find public websites or indexed descriptions about a subject. Args: query. Use public search terms only. Distinguish search results from pages you read with browse."
            }
            Self::Form => {
                "Ask for values to fill a website form. Args: url, optional zero-based index."
            }
            Self::Input => {
                "Create a form for ordinary preferences or demonstrate a form on request. Args: purpose, optional surface (auto or mini_app), and optional fields array (name, label, type, required, options). Pass fields directly when known. Choose mini_app for a form, even for one select field. In auto mode, one select with 2–6 short options can be answered in chat. Never request credentials, payment or private data."
            }
            Self::AgentSpec => {
                "Propose an editable saved agent in the Mini App. Args: name, instructions."
            }
            Self::Schedule => {
                "Schedule the Chief to send a reminder or carry out a task when due. Args: kind (reminder or task), title, input (a self-contained prompt for the run), exactly one of at (RFC 3339 with UTC offset) or cron (5 fields in the user's timezone)."
            }
            Self::ListSchedules => "List the user's scheduled reminders and tasks. Args: {}.",
            Self::CancelSchedule => {
                "Cancel a requested upcoming schedule. Args: id from list_schedules."
            }
            Self::Files => "List or read task files. Args: optional name for reading a file.",
            Self::Python => {
                "Run offline Python on the current task's files. Args: code. Use filenames in the working directory."
            }
            Self::SendReply => {
                "Send the final text reply with optional tappable suggested replies."
            }
            Self::SendCard => {
                "Send a Telegram card or widget to the current chat, using a title, text, list items, and optional tappable suggested replies."
            }
            Self::React => {
                "Set one native Telegram emoji reaction on the latest user message. Use an emoji such as 👍, ❤️, 👀, 🎉, or 🙏."
            }
        }
    }

    fn input_schema(&self) -> Value {
        let text = json!({"type":"string"});
        let suggestions =
            json!({"type":"array","maxItems":3,"items":{"type":"string","maxLength":48}});
        let (properties, required): (Value, &[&str]) = match self {
            Self::Remember => (
                json!({"text":text,"scope":{"type":"string","enum":["private","group"]},"agent":text}),
                &["text"],
            ),
            Self::RecallMemory => (json!({"query":text,"agent":text}), &["query"]),
            Self::SetGoal => (json!({"text":text}), &["text"]),
            Self::Delegate => (
                json!({"brief":text,"agent":text,"handoff":{"type":"string","maxLength":180}}),
                &["brief"],
            ),
            Self::Discover => (json!({"name":text}), &["name"]),
            Self::Connect => (
                json!({"name":text,"endpoint":text,"transport":{"type":"string","enum":["auto","streamable-http","sse","stdio"]}}),
                &["name", "endpoint"],
            ),
            Self::ListMcp => (json!({"connector":text,"search":text}), &["connector"]),
            Self::CallMcp => (
                json!({"connector":text,"tool":text,"arguments":{"type":"object"}}),
                &["connector", "tool", "arguments"],
            ),
            Self::Browse => (json!({"url":text}), &["url"]),
            Self::SearchWeb => (json!({"query":text}), &["query"]),
            Self::Form => (
                json!({"url":text,"index":{"type":"integer","minimum":0}}),
                &["url"],
            ),
            Self::Input => (
                json!({
                    "purpose":text,
                    "surface":{"type":"string","enum":["auto","mini_app"]},
                    "fields":{"type":"array","maxItems":8,"items":{
                        "type":"object","required":["name","label","type"],"additionalProperties":false,
                        "properties":{"name":text,"label":text,"hint":text,
                            "type":{"type":"string","enum":["text","textarea","number","date","select","checkbox"]},
                            "options":{"type":"array","items":text},"required":{"type":"boolean"}}
                    }}
                }),
                &["purpose"],
            ),
            Self::AgentSpec => (
                json!({"name":text,"instructions":text}),
                &["name", "instructions"],
            ),
            Self::Schedule => (
                json!({"kind":{"type":"string","enum":["reminder","task"]},"title":text,"input":text,"at":text,"cron":text}),
                &["kind", "title", "input"],
            ),
            Self::ListSchedules => (json!({}), &[]),
            Self::CancelSchedule => (json!({"id":text}), &["id"]),
            Self::Files => (json!({"name":text}), &[]),
            Self::Python => (json!({"code":text}), &["code"]),
            Self::SendReply => (
                json!({"text":text,"suggested_replies":suggestions}),
                &["text"],
            ),
            Self::SendCard => (
                json!({"text":text,"title":text,"items":{"type":"array","maxItems":4,"items":text},"suggested_replies":suggestions}),
                &[],
            ),
            Self::React => (
                json!({"emoji":{"type":"string","enum":crate::store::REACTION_EMOJIS}}),
                &["emoji"],
            ),
        };
        json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
    }

    async fn inspect(&self, ctx: &ToolContext<'_>, args: &Value) -> Result<Intent> {
        ensure!(
            args.is_object() && args.to_string().len() < 30000,
            "invalid tool arguments"
        );
        let mut destination = "TinyBot".into();
        let mut detail = self.name().to_owned();
        let risk = match self {
            Self::Browse | Self::Form => {
                ensure!(ctx.owner, "browser access is restricted on shared agents");
                let raw = string(args, "url", 2048)?;
                let (url, risk) = if matches!(self, Self::Browse) {
                    (net::website(raw)?, Risk::PublicRead)
                } else {
                    (net::https(raw)?, Risk::Read)
                };
                destination = url.origin().ascii_serialization();
                risk
            }
            Self::SearchWeb => {
                ensure!(ctx.owner, "web search is restricted on shared agents");
                string(args, "query", 240)?;
                destination = "Public web search".into();
                Risk::PublicRead
            }
            Self::Discover => {
                ensure!(ctx.owner, "only the agent owner can connect MCPs");
                destination = "Official MCP Registry".into();
                string(args, "name", 2048)?;
                Risk::Read
            }
            Self::Connect => {
                ensure!(ctx.owner, "only the agent owner can connect MCPs");
                string(args, "endpoint", 2048)?;
                string(args, "name", 100)?;
                let candidate: mcp::Candidate = serde_json::from_value(args.clone())?;
                destination = candidate.endpoint()?;
                Risk::Write
            }
            Self::ListMcp | Self::CallMcp => {
                let connector = ctx.connector(args)?;
                destination = net::https(&connector.endpoint).map_or_else(
                    |_| connector.endpoint.clone(),
                    |url| url.origin().ascii_serialization(),
                );
                if matches!(self, Self::ListMcp) {
                    Risk::Read
                } else {
                    let tool = string(args, "tool", 128)?;
                    ensure!(
                        tool.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
                        "invalid MCP tool name"
                    );
                    ensure!(
                        args["arguments"].is_object(),
                        "MCP arguments must be an object"
                    );
                    detail = format!("MCP tool {tool}");
                    if !ctx.owner {
                        let agent = ctx.task.agent.as_deref().context("agent required")?;
                        ensure!(
                            ctx.app
                                .mcp
                                .allowed_shared(
                                    agent,
                                    &ctx.task.user,
                                    connector,
                                    tool,
                                    &args["arguments"]
                                )
                                .await?,
                            "the owner has not shared that exact read"
                        );
                    }
                    if ctx.app.mcp.verified_read(connector, tool) {
                        Risk::Read
                    } else {
                        Risk::Write
                    }
                }
            }
            Self::Schedule | Self::ListSchedules | Self::CancelSchedule => {
                ensure!(
                    ctx.owner && ctx.task.agent.is_none(),
                    "use your personal Chief to manage schedules"
                );
                if matches!(self, Self::Schedule) {
                    let request: schedules::Request = serde_json::from_value(args.clone())?;
                    schedules::validate(
                        &request,
                        schedules::timezone(&ctx.app.db, &ctx.task.user).await?,
                        now(),
                    )?;
                    if request.kind == "task" {
                        Risk::Write
                    } else {
                        Risk::Local
                    }
                } else {
                    Risk::Local
                }
            }
            Self::AgentSpec => {
                ensure!(
                    ctx.owner && ctx.task.agent.is_none(),
                    "use your personal Chief for this action"
                );
                Risk::Local
            }
            _ => Risk::Local,
        };
        Ok(Intent {
            risk,
            destination,
            detail,
        })
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> Result<Outcome> {
        effects::execute(*self, ctx, args).await
    }
}
