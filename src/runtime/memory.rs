mod smart;

pub(super) use smart::{for_chief, for_helper};

use super::{ToolContext, string};
use crate::store::memory;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

fn worker<'a>(ctx: &'a ToolContext<'_>, args: &'a Value) -> Result<Option<&'a str>> {
    if ctx.parent_task.is_some() {
        ensure!(
            args["agent"].is_null(),
            "helpers can use only their own memory"
        );
        return Ok(Some(
            ctx.task.agent.as_deref().context("saved helper required")?,
        ));
    }
    if args["agent"].is_null() {
        Ok(None)
    } else {
        Ok(Some(string(args, "agent", 64)?))
    }
}

pub(super) async fn save(ctx: &ToolContext<'_>, args: &Value) -> Result<Value> {
    let text = string(args, "text", 1200)?;
    let helper = worker(ctx, args)?;
    let group = match args.get("scope") {
        None | Some(Value::Null) => false,
        Some(value) if value == "private" => false,
        Some(value) if value == "group" => true,
        _ => anyhow::bail!("choose private or group memory"),
    };
    ensure!(
        !group || helper.is_none(),
        "use the Chief to save group memory"
    );
    let entry = memory::remember(
        &ctx.app.db,
        ctx.parent_task.unwrap_or(ctx.task),
        helper,
        group,
        text,
    )
    .await?;
    Ok(json!({"saved":true,"scope":entry.scope}))
}

pub(super) async fn recall(ctx: &ToolContext<'_>, args: &Value) -> Result<Value> {
    let query = string(args, "query", 160)?;
    let helper = worker(ctx, args)?;
    let notes = memory::recall(
        &ctx.app.db,
        ctx.parent_task.unwrap_or(ctx.task),
        helper,
        query,
    )
    .await?;
    Ok(json!({"memories":notes}))
}
