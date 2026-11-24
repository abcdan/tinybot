use super::{Intent, Outcome, Risk, Tool, ToolContext, string};
use crate::store;
use anyhow::{Result, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};

pub(super) struct CompleteTask;

#[async_trait]
impl Tool for CompleteTask {
    fn name(&self) -> &'static str {
        "complete_task"
    }

    fn describe(&self) -> &'static str {
        "Mark the current private task complete only after the latest Telegram message explicitly confirms it is done. Quote that message exactly and optionally acknowledge completion in the user's language."
    }

    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "confirmation":{"type":"string","description":"Exact text of the latest user's completion confirmation."},
            "reply":{"type":"string","maxLength":320}
        },"required":["confirmation"],"additionalProperties":false})
    }

    async fn inspect(&self, ctx: &ToolContext<'_>, args: &Value) -> Result<Intent> {
        ensure!(
            ctx.allows(self.name()),
            "task completion requires a private Chief task"
        );
        let confirmation = string(args, "confirmation", 8_000)?.trim();
        let latest: Option<(String, Option<i64>)> = sqlx::query_as(
            "SELECT e.body,e.telegram_message_id FROM tasks t JOIN events e ON e.task=t.id
             WHERE t.id=? AND t.user=? AND t.revision=? AND t.state='running' AND t.chief_chat=1
               AND t.schedule IS NULL AND e.role='user' ORDER BY e.id DESC LIMIT 1",
        )
        .bind(&ctx.task.id)
        .bind(&ctx.task.user)
        .bind(ctx.task.revision)
        .fetch_optional(&ctx.app.db)
        .await?;
        ensure!(
            latest
                .is_some_and(|(text, message)| message.is_some_and(|id| id > 0)
                    && text.trim() == confirmation),
            "completion must quote the latest Telegram message"
        );
        Ok(Intent {
            risk: Risk::Local,
            destination: "Your task list".into(),
            detail: "Mark the current task complete".into(),
        })
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> Result<Outcome> {
        let reply = args["reply"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        ensure!(
            reply.is_none_or(|text| text.chars().count() <= 320),
            "completion reply is too long"
        );
        ensure!(
            store::task_views::complete_with_reply(
                &ctx.app.db,
                &ctx.task.user,
                &ctx.task.id,
                ctx.task.revision,
                reply,
            )
            .await?,
            "the task could not be marked complete"
        );
        Ok(Outcome::Pause)
    }
}
