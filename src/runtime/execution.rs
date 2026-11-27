use super::delegation::delegate;
use super::{Call, Outcome, ToolContext};
use crate::{audit, security::clip, store};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

pub(super) enum ApprovalProgress {
    Continue,
    Settle,
    Finished,
}

async fn continue_if_current(ctx: &ToolContext<'_>) -> Result<ApprovalProgress> {
    Ok(if store::current(&ctx.app.db, ctx.task).await? {
        ApprovalProgress::Continue
    } else {
        ApprovalProgress::Settle
    })
}

pub(super) async fn resume_approval(ctx: &ToolContext<'_>) -> Result<ApprovalProgress> {
    let app = ctx.app;
    let docblock_save = store::docblock_saves::dedicated(&app.db, ctx.task).await?;
    let Some(pending) = store::claim_approval(&app.db, ctx.task).await? else {
        if docblock_save {
            store::docblock_saves::dismiss(&app.db, ctx.task).await?;
            return Ok(ApprovalProgress::Settle);
        }
        return Ok(ApprovalProgress::Continue);
    };
    let payload: Value = serde_json::from_str(&pending.payload)?;
    if !super::automatic::current(ctx, &payload).await?
        || !super::receipts::current(ctx, &payload).await?
    {
        store::finish_action(
            &app.db,
            ctx.task,
            &pending.id,
            "The action policy or instruction changed before execution. Nothing was sent.",
            false,
        )
        .await?;
        return continue_if_current(ctx).await;
    }
    if docblock_save {
        super::docblock_save::resume(ctx, &pending, &payload).await?;
        return Ok(ApprovalProgress::Settle);
    }
    let (result, announce) = match pending.kind.as_str() {
        "approve" if payload["calendar_event"] == true => {
            super::calendar::resume(ctx, &pending, &payload).await?;
            return continue_if_current(ctx).await;
        }
        super::email::KIND => {
            super::email::resume(ctx, &pending, &payload).await?;
            return continue_if_current(ctx).await;
        }
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
            let call: Call = if let Some(sealed) = payload["sealed_call"].as_str() {
                serde_json::from_slice(&app.vault.open(
                    &format!("automatic-call:{}:{}", ctx.task.user, pending.id),
                    sealed,
                )?)?
            } else {
                serde_json::from_value(payload["call"].clone())?
            };
            audit::record("approval", "external_action", "approved", Value::Null).await;
            if let Some(scope) = payload.get("mcp_approval") {
                let scope: store::McpApprovalScope = serde_json::from_value(scope.clone())?;
                let current = call.name == "call_mcp"
                    && super::mcp_approval::scope(ctx, &call.args)
                        .is_ok_and(|value| value == scope)
                    && store::mcp_approval_current(&app.db, &ctx.task.user, &scope).await?;
                if !current {
                    store::finish_action(&app.db, ctx.task, &pending.id, "The MCP connection changed. Ask for a new approval before executing this call.", false).await?;
                    return continue_if_current(ctx).await;
                }
            }
            let media = super::media_tools::is_media_tool(&call.name);
            let result = execute_approved(ctx, call).await;
            if media && !store::current(&app.db, ctx.task).await? {
                store::interrupt_action(&app.db, ctx.task, &pending.id).await?;
                return Ok(ApprovalProgress::Settle);
            }
            let announce = !media
                && (result.is_err()
                    || (payload["receipt"] != true
                        && payload.get("automatic_connection").is_none()));
            (result, announce)
        }
    };
    if finish(ctx, &pending.id, result, announce).await? {
        return Ok(ApprovalProgress::Finished);
    }
    continue_if_current(ctx).await
}

pub(super) async fn execute_approved(ctx: &ToolContext<'_>, payload: Call) -> Result<Outcome> {
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

/// Saves the action result. Returns true when the result has already settled the turn.
pub(super) async fn finish(
    ctx: &ToolContext<'_>,
    id: &str,
    result: Result<Outcome>,
    announce: bool,
) -> Result<bool> {
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
        Ok(Outcome::Media(media)) => {
            let app = ctx.app;
            let queued = match store::outputs::queue_media(
                &app.db, &app.vault, ctx.task, id, &media,
            )
            .await
            {
                Ok(queued) => queued,
                Err(_) if !store::current(&app.db, ctx.task).await? => {
                    store::interrupt_action(&app.db, ctx.task, id).await?;
                    return Ok(false);
                }
                Err(error) => return Err(error),
            };
            if queued {
                app.delivery_ready.notify_one();
            }
            audit::record(
                "tool",
                media.tool,
                "completed",
                json!({"queued":queued,"cookie_dialog":media.cookie_dialog}),
            )
            .await;
            return Ok(queued && media.finish_turn);
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
    Ok(false)
}
