use super::{Intent, Outcome, Risk, Tool, ToolContext, string};
use crate::store;
use anyhow::{Result, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};

pub(super) struct SendTaskFile;

#[async_trait]
impl Tool for SendTaskFile {
    fn name(&self) -> &'static str {
        "send_task_file"
    }

    fn describe(&self) -> &'static str {
        "When the user asks for a generated file, deliver an existing task file to their private Telegram chat. Args: name, a CSV, Markdown, JSON, or text filename from this task."
    }

    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false})
    }

    async fn inspect(&self, ctx: &ToolContext<'_>, args: &Value) -> Result<Intent> {
        ensure!(
            ctx.owner && ctx.parent_task.is_none() && ctx.task.agent.is_none(),
            "only the personal Chief can deliver a task file"
        );
        let name = string(args, "name", 96)?;
        ensure!(
            name.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
                && !name.starts_with('.')
                && name.rsplit_once('.').is_some_and(|(_, extension)| {
                    matches!(
                        extension.to_ascii_lowercase().as_str(),
                        "csv" | "md" | "markdown" | "json" | "txt"
                    )
                }),
            "use a supported task filename"
        );
        Ok(Intent {
            risk: Risk::Local,
            destination: "Your private Telegram chat".into(),
            detail: "Deliver task file".into(),
        })
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> Result<Outcome> {
        let name = string(&args, "name", 96)?;
        let queued =
            store::outputs::queue_document(&ctx.app.db, &ctx.app.vault, ctx.task, name).await?;
        Ok(Outcome::Continue(json!({"queued":queued,"name":name})))
    }
}
