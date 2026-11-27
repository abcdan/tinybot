use super::{Intent, MAX_HANDOFF_CHARS, Outcome, Risk, Tool, ToolContext, effects, string};
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
    ConnectDocblock,
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
    SaveRoutine,
    RunRoutine,
    RecordRoutineStep,
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
        Builtin::ConnectDocblock,
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
        Builtin::SaveRoutine,
        Builtin::RunRoutine,
        Builtin::RecordRoutineStep,
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
            Self::ConnectDocblock => "connect_docblock",
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
            Self::SaveRoutine => "save_routine",
            Self::RunRoutine => "run_routine",
            Self::RecordRoutineStep => "record_routine_step",
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
            Self::SetGoal => {
                "Save a revised task goal at the user's request. The initial task goal is already recorded."
            }
            Self::Delegate => {
                "Ask another agent to carry out a self-contained brief and return findings to you. Choose a matching saved agent by ID when available. Write the handoff as one short, casual sentence in the user's language saying who will do what. Name the selected helper. Speak as yourself or make the helper the subject. Skip the detailed plan and any promise to report back. In Dutch, for example, 'Ik ga het via de research bot opzoeken' or 'Research gaat de vacatures uitzoeken'."
            }
            Self::Discover => "Find official remote MCP endpoints. Args: name.",
            Self::Connect => {
                "Prepare a discovered connector after review. Args: name, endpoint, transport (auto, streamable-http, sse or stdio)."
            }
            Self::ConnectDocblock => {
                "Open Docblock sign-in when the user requests Docblock and no usable connection exists. Args: optional write=true when direct write access is needed."
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
                "Schedule a reminder or a task. Args: kind (reminder or task), title, input (exact text to send for a reminder, a self-contained instruction for a task), exactly one of at (RFC 3339 with UTC offset) or cron (5 fields in the user's timezone)."
            }
            Self::ListSchedules => "List the user's scheduled reminders and tasks. Args: {}.",
            Self::CancelSchedule => {
                "Cancel a requested upcoming schedule. Args: id from list_schedules."
            }
            Self::SaveRoutine => {
                "On explicit request, propose a reusable routine for review. Args: title, input (the overall goal), ordered steps (instruction and optional owned helper agent ID), and optional at or cron. Omit timing for a manual routine."
            }
            Self::RunRoutine => {
                "Queue a saved routine now on the user's explicit request. Args: id and revision from list_schedules."
            }
            Self::RecordRoutineStep => {
                "Finish the current routine step. Pass its zero-based index (the number of recorded results). For a Chief step, first do the work, then pass the result. For a helper step, omit result so the runtime can dispatch the saved helper."
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
                json!({"brief":text,"agent":text,"handoff":{"type":"string","maxLength":MAX_HANDOFF_CHARS}}),
                &["brief"],
            ),
            Self::Discover => (json!({"name":text}), &["name"]),
            Self::Connect => (
                json!({"name":text,"endpoint":text,"transport":{"type":"string","enum":["auto","streamable-http","sse","stdio"]}}),
                &["name", "endpoint"],
            ),
            Self::ConnectDocblock => (json!({"write":{"type":"boolean"}}), &[]),
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
            Self::SaveRoutine => (
                json!({
                    "title":text,"input":text,"at":text,"cron":text,
                    "steps":{"type":"array","minItems":1,"maxItems":6,"items":{
                        "type":"object","properties":{"instruction":{"type":"string","maxLength":600},"agent":text},
                        "required":["instruction"],"additionalProperties":false
                    }}
                }),
                &["title", "input", "steps"],
            ),
            Self::RunRoutine => (
                json!({"id":text,"revision":{"type":"integer","minimum":0}}),
                &["id", "revision"],
            ),
            Self::RecordRoutineStep => (
                json!({"index":{"type":"integer","minimum":0,"maximum":5},"result":{"type":"string","maxLength":1200}}),
                &["index"],
            ),
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
            Self::ConnectDocblock => {
                ensure!(
                    ctx.owner && ctx.task.agent.is_none() && ctx.parent_task.is_none(),
                    "use your Chief to sign in to Docblock"
                );
                Risk::Local
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
                    super::calendar::require_review_path(ctx, connector, tool).await?;
                    detail = format!("MCP tool {tool}");
                    if mcp::docblock::is_endpoint(&connector.endpoint)
                        && mcp::docblock::reviewed_write(tool)
                    {
                        ensure!(
                            ctx.owner && ctx.task.agent.is_none() && ctx.parent_task.is_none(),
                            "use your Chief to change a Docblock note"
                        );
                        if connector.auth == "oauth" {
                            let access = ctx.app.mcp.docblock_access(connector);
                            ensure!(
                                access == Some("write")
                                    || (tool != "create_share" && access == Some("propose")),
                                "Docblock direct write access is missing. Explain the current access and offer Docblock sign-in in Connections."
                            );
                        }
                        mcp::docblock::review(tool, &args["arguments"])?;
                    }
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
            Self::Schedule
            | Self::ListSchedules
            | Self::CancelSchedule
            | Self::SaveRoutine
            | Self::RunRoutine => {
                ensure!(
                    ctx.owner && ctx.task.agent.is_none(),
                    "use your personal Chief to manage schedules"
                );
                if matches!(self, Self::SaveRoutine) {
                    ensure!(
                        args.to_string().len() <= 2800,
                        "shorten the routine so its entire plan fits the review"
                    );
                    let _: schedules::RoutineRequest = serde_json::from_value(args.clone())?;
                    detail = "save a routine".into();
                    Risk::Write
                } else if matches!(self, Self::RunRoutine) {
                    string(args, "id", 64)?;
                    args["revision"]
                        .as_u64()
                        .context("invalid routine revision")?;
                    Risk::Local
                } else if matches!(self, Self::Schedule) {
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
            Self::RecordRoutineStep => {
                ensure!(ctx.owner && ctx.task.agent.is_none(), "routine unavailable");
                let progress = schedules::progress(&ctx.app.db, ctx.task)
                    .await?
                    .context("routine unavailable")?;
                let step = progress.current.context("all routine steps are complete")?;
                ensure!(
                    args["index"].as_u64() == Some(progress.results.len() as u64),
                    "finish the current routine step first"
                );
                if step.agent.is_none() {
                    string(args, "result", 1200)?;
                }
                Risk::Local
            }
            Self::SendReply | Self::SendCard => {
                ensure!(
                    schedules::progress(&ctx.app.db, ctx.task)
                        .await?
                        .is_none_or(|progress| progress.current.is_none()),
                    "complete the remaining routine steps before sending the final reply"
                );
                Risk::Local
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
