use super::{Intent, Outcome, Risk, Tool, ToolContext, effects, string};
use crate::{mcp, net, schedules, security::now};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde_json::Value;

#[derive(Clone, Copy)]
pub(super) enum Builtin {
    Remember,
    SetGoal,
    Delegate,
    Discover,
    Connect,
    ListMcp,
    CallMcp,
    Browse,
    Form,
    Input,
    AgentSpec,
    Schedule,
    ListSchedules,
    CancelSchedule,
    Files,
    Python,
}

pub fn tools() -> Vec<Box<dyn Tool>> {
    [
        Builtin::Remember,
        Builtin::SetGoal,
        Builtin::Delegate,
        Builtin::Discover,
        Builtin::Connect,
        Builtin::ListMcp,
        Builtin::CallMcp,
        Builtin::Browse,
        Builtin::Form,
        Builtin::Input,
        Builtin::AgentSpec,
        Builtin::Schedule,
        Builtin::ListSchedules,
        Builtin::CancelSchedule,
        Builtin::Files,
        Builtin::Python,
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
            Self::SetGoal => "set_goal",
            Self::Delegate => "delegate",
            Self::Discover => "discover_mcp",
            Self::Connect => "connect_mcp",
            Self::ListMcp => "list_mcp_tools",
            Self::CallMcp => "call_mcp",
            Self::Browse => "browse",
            Self::Form => "request_form",
            Self::Input => "request_input",
            Self::AgentSpec => "propose_agent",
            Self::Schedule => "schedule",
            Self::ListSchedules => "list_schedules",
            Self::CancelSchedule => "cancel_schedule",
            Self::Files => "task_files",
            Self::Python => "python",
        }
    }

    fn describe(&self) -> &'static str {
        match self {
            Self::Remember => "Remember a durable user preference privately. Args: text.",
            Self::SetGoal => "Record the current task goal. Args: text.",
            Self::Delegate => {
                "Delegate a bounded brief. Args: brief, optional agent (saved agent ID)."
            }
            Self::Discover => "Find official remote MCP endpoints. Args: name.",
            Self::Connect => {
                "Prepare a discovered connector after review. Args: name, endpoint, transport (auto, streamable-http, sse or stdio)."
            }
            Self::ListMcp => {
                "Get available MCP tool names and schemas. Args: connector, optional search."
            }
            Self::CallMcp => "Call a connector tool. Args: connector, tool, arguments object.",
            Self::Browse => "Read visible text from a public HTTPS page. Args: url.",
            Self::Form => {
                "Ask for values to fill a website form. Args: url, optional zero-based index."
            }
            Self::Input => {
                "Generate a Mini App form for ordinary preferences and task choices. Args: purpose. Never request credentials, payment or private website data."
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
        }
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
                let url = net::https(string(args, "url", 2048)?)?;
                destination = url.origin().ascii_serialization();
                Risk::Read
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
            Self::AgentSpec | Self::Remember => {
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
