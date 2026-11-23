use super::delegation::delegate;
use super::{Call, Outcome, ToolContext};
use crate::{audit, security::clip, store};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

/// Runs a pending approval, if present, and reports whether the turn can continue.
pub(super) async fn resume_approval(ctx: &ToolContext<'_>) -> Result<bool> {
    let app = ctx.app;
    let Some(pending) = store::claim_approval(&app.db, ctx.task).await? else {
        return Ok(true);
    };
    let payload: Value = serde_json::from_str(&pending.payload)?;
    let (result, announce) = match pending.kind.as_str() {
        "form-submit" => {
            audit::record("tool", "form_submission", "approved", Value::Null).await;
            audit::tool_started("form_submission", &Value::Null).await;
            let result = async {
                ensure!(
                    ctx.still_allows("request_form").await?,
                    "task or authorization changed"
                );
                let values = app.forms.submission(&pending.id).await?;
                app.workers.browser("submit", &values).await
            }
            .await;
            let result = result.map(Outcome::Continue);
            audit::tool_finished("form_submission", &result).await;
            (result, true)
        }
        "agent-spec" => {
            audit::record("tool", "propose_agent", "approved", Value::Null).await;
            (
                Ok(Outcome::Continue(json!({"agent_saved":payload["name"]}))),
                false,
            )
        }
        "chief-delegate" => (delegate(ctx, &payload).await.map(Outcome::Continue), false),
        _ => {
            let call: Call = serde_json::from_value(payload["call"].clone())?;
            audit::record("approval", "external_action", "approved", Value::Null).await;
            if let Some(scope) = payload.get("mcp_approval") {
                let scope: store::McpApprovalScope = serde_json::from_value(scope.clone())?;
                let current = call.name == "call_mcp"
                    && super::mcp_approval::scope(ctx, &call.args)
                        .is_ok_and(|value| value == scope)
                    && store::mcp_approval_current(&app.db, &ctx.task.user, &scope).await?;
                if !current {
                    store::finish_action(&app.db, ctx.task, &pending.id, "The MCP connection changed. Ask for a new approval before executing this call.", false).await?;
                    return store::current(&app.db, ctx.task).await;
                }
            }
            (execute_approved(ctx, call).await, true)
        }
    };
    finish(ctx, &pending.id, result, announce).await?;
    store::current(&app.db, ctx.task).await
}

async fn execute_approved(ctx: &ToolContext<'_>, payload: Call) -> Result<Outcome> {
    ensure!(
        ctx.owner && ctx.still_allows(&payload.name).await?,
        "approval access changed"
    );
    let tool = ctx
        .app
        .tools
        .iter()
        .find(|tool| tool.name() == payload.name)
        .context("tool unavailable")?;
    tool.inspect(ctx, &payload.args).await?;
    audit::tool_started(tool.name(), &payload.args).await;
    let result = tool.execute(ctx, payload.args).await;
    audit::tool_finished(tool.name(), &result).await;
    result
}

pub(super) async fn finish(
    ctx: &ToolContext<'_>,
    id: &str,
    result: Result<Outcome>,
    announce: bool,
) -> Result<()> {
    match result {
        Ok(Outcome::Continue(value)) => {
            let text = value.to_string();
            store::finish_action(&ctx.app.db, ctx.task, id, &text, false).await?;
            if announce {
                let message = if value.get("submitted").and_then(Value::as_bool) == Some(true) {
                    "The website accepted the form submission request. I’ll continue with the result."
                } else {
                    "The approved action returned. I’ll continue with the result."
                };
                sqlx::query("INSERT INTO outbox(user,task,agent,body) VALUES(?,?,?,?)")
                    .bind(&ctx.task.user)
                    .bind(&ctx.task.id)
                    .bind(&ctx.task.agent)
                    .bind(message)
                    .execute(&ctx.app.db)
                    .await?;
            }
        }
        Ok(Outcome::Pause) => {
            store::finish_action(
                &ctx.app.db,
                ctx.task,
                id,
                "Paused for a new request.",
                false,
            )
            .await?;
        }
        Err(error) => {
            let text = if announce {
                format!(
                    "The action could not be verified. {} Check the destination, then send /reconcile to continue.",
                    clip(&error.to_string(), 280)
                )
            } else {
                format!("Tool did not complete. {}", clip(&error.to_string(), 280))
            };
            if announce {
                store::uncertain_action(&ctx.app.db, ctx.task, id, &text).await?;
            } else {
                store::finish_action(&ctx.app.db, ctx.task, id, &text, false).await?;
            }
        }
    }
    Ok(())
}
