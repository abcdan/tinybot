use super::review::{Verdict, review};
use super::{ENGINE, Outcome, Risk, Step, ToolContext, connector_context, descriptions, string};
use crate::{
    model::json_reply,
    security::clip,
    store::{self, Agent},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

pub(super) async fn delegate(ctx: &ToolContext<'_>, args: &Value) -> Result<Value> {
    let brief = string(args, "brief", 4000)?;
    let saved = if let Some(reference) = args["agent"].as_str() {
        ensure!(ctx.owner, "only the owner can delegate to saved agents");
        Some(
            sqlx::query_as::<_, Agent>("SELECT * FROM agents WHERE id=? AND owner=?")
                .bind(reference)
                .bind(&ctx.task.user)
                .fetch_optional(&ctx.app.db)
                .await?
                .context("saved agent unavailable")?,
        )
    } else {
        None
    };
    let allowed = if let Some(saved) = &saved {
        serde_json::from_str::<Vec<String>>(&saved.tools)?
    } else {
        vec!["list_mcp_tools".into(), "call_mcp".into(), "browse".into()]
    };
    let allowed: Vec<_> = ctx
        .app
        .tools
        .iter()
        .filter(|tool| {
            allowed.iter().any(|name| name == tool.name())
                && ctx.allows(tool.name())
                && matches!(tool.name(), "list_mcp_tools" | "call_mcp" | "browse")
        })
        .collect();
    let instruction = format!(
        "{ENGINE} You are a temporary helper. Your reply goes to the Chief. Omit suggested_replies. If input or approval is needed, describe precisely what the Chief should ask. Saved role: {}",
        saved
            .as_ref()
            .map_or("helper", |agent| agent.instructions.as_str())
    );
    let mut history = Vec::new();
    let model = ctx.app.models.provider(&ctx.task.user, false).await?;
    for _ in 0..4 {
        ensure!(
            store::current(&ctx.app.db, ctx.task).await?,
            "task was updated during delegation"
        );
        let step: Step = json_reply(
            &*model,
            &instruction,
            &json!({"brief":brief,"tools":descriptions(&allowed),
            "connectors":connector_context(ctx),"observations":history}),
        )
        .await?;
        ensure!(
            store::current(&ctx.app.db, ctx.task).await?,
            "task was updated during delegation"
        );
        match step {
            Step::Reply { text, .. } | Step::Widget { text, .. } => {
                return Ok(json!({"helper_reply":clip(&text,6000)}));
            }
            Step::Tool { name, args } => {
                let Some(tool) = allowed.iter().find(|tool| tool.name() == name) else {
                    history.push(json!({"error":"Tool unavailable to this helper"}));
                    continue;
                };
                match tool.inspect(ctx, &args).await {
                    Ok(intent) if matches!(intent.risk, Risk::Read | Risk::PublicRead) => {
                        if review(ctx, &intent).await?.verdict != Verdict::Allow {
                            return Ok(
                                json!({"chief_request":format!("Review {} at {}",intent.detail,intent.destination)}),
                            );
                        }
                        ensure!(
                            ctx.still_allows(tool.name()).await?,
                            "task permissions changed during delegation"
                        );
                        tool.inspect(ctx, &args).await?;
                        if let Outcome::Continue(value) = tool.execute(ctx, args).await? {
                            history.push(value);
                        }
                    }
                    _ => {
                        return Ok(
                            json!({"chief_request":"A requested tool needs your permissions or approval.","tool":name}),
                        );
                    }
                }
            }
        }
    }
    Ok(json!({"helper_reply":"The helper reached its step limit.","observations":history}))
}
