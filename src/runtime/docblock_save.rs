use super::{Call, Outcome, ToolContext, execution, mcp_approval};
use crate::{
    audit,
    mcp::docblock,
    store::{self, McpApprovalScope, Pending, docblock_saves as saves},
};
use anyhow::{Context, Result};
use serde_json::Value;

pub(super) async fn resume(
    ctx: &ToolContext<'_>,
    pending: &Pending,
    payload: &Value,
) -> Result<()> {
    let app = ctx.app;
    let db = &app.db;
    let call: Call = serde_json::from_value(payload["call"].clone())?;
    let scope: McpApprovalScope = serde_json::from_value(payload["mcp_approval"].clone())?;
    let finish = |status, url, message| saves::finish(db, ctx.task, pending, status, url, message);
    let arguments = &call.args["arguments"];
    let workspace = arguments["workspace"]
        .as_str()
        .context("missing Docblock workspace")?;
    let path = arguments["path"]
        .as_str()
        .context("missing Docblock path")?
        .to_owned();
    if call.name != "call_mcp"
        || scope.tool != "write_note"
        || !mcp_approval::scope(ctx, &call.args).is_ok_and(|value| value == scope)
        || !store::mcp_approval_current(db, &ctx.task.user, &scope).await?
        || !saves::source_current(db, ctx.task, pending).await?
    {
        return finish("failed", None, "The task result or Docblock access changed. Refresh the task before reviewing a new save.").await;
    }

    let connector = ctx.connector(&call.args)?;
    let available = docblock::is_endpoint(&connector.endpoint)
        && app
            .mcp
            .docblock_folders(connector, workspace)
            .await
            .is_ok_and(|folders| {
                path.rsplit_once('/')
                    .is_none_or(|(folder, _)| folders.iter().any(|name| name == folder))
            });
    if !available
        || !app
            .mcp
            .docblock_note_absent(connector, arguments)
            .await
            .unwrap_or(false)
    {
        return finish("failed", None, "TinyBot could not verify that the selected Docblock location is available for a new note.").await;
    }
    if !store::mcp_approval_current(db, &ctx.task.user, &scope).await?
        || !saves::mark_executing(db, ctx.task, pending).await?
    {
        return finish("failed", None, "The task result or Docblock access changed. Refresh the task before reviewing a new save.").await;
    }

    let workspace = workspace.to_owned();
    audit::record("approval", "docblock_task_save", "approved", Value::Null).await;
    let result = match execution::execute_approved(ctx, call).await {
        Ok(Outcome::Continue(value)) => value["result"]
            .as_str()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok()),
        _ => None,
    };
    if let Some(result) = result {
        if result["docblock_refused"] == true {
            return finish(
                "failed",
                None,
                "Docblock declined the note. Check the workspace permissions before trying again.",
            )
            .await;
        }
        let note = &result["note"];
        if result["applied"] == true && note["workspaceId"] == workspace && note["path"] == path {
            let url = note["url"].as_str();
            let message = url.map_or_else(
                || "Saved a private note in Docblock.".to_owned(),
                |url| format!("Saved a private note in Docblock.\n{url}"),
            );
            return saves::finish(db, ctx.task, pending, "saved", url, &message).await;
        }
        if result["applied"] == false
            && let Some(url) = result["proposal_url"].as_str()
        {
            return saves::finish(
                db,
                ctx.task,
                pending,
                "proposed",
                Some(url),
                &format!("The note is awaiting review in Docblock.\n{url}"),
            )
            .await;
        }
    }
    store::uncertain_action(db, ctx.task, &pending.id,
        "TinyBot could not verify whether Docblock saved the note. Check the destination in Docblock before taking another action.").await
}
