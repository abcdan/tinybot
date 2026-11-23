use super::{Call, Outcome, ToolContext};
use crate::{audit, security::clip, store};
use anyhow::{Context, Result, ensure};
use serde_json::Value;

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
