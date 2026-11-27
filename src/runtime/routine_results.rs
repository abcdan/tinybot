use super::{Intent, Outcome, Risk, Tool, ToolContext, settings};
use crate::schedules::routine_results;
use anyhow::{Result, ensure};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

pub(super) struct ReadRoutineResults;

const READ_ROUTINE_RESULTS: &str = "read_routine_results";

pub(super) fn is_tool(name: &str) -> bool {
    name == READ_ROUTINE_RESULTS
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schedule: Option<String>,
    limit: Option<i64>,
}

impl Request {
    fn parse(args: Value) -> Result<Self> {
        let request: Self = serde_json::from_value(args)?;
        ensure!(
            request
                .schedule
                .as_ref()
                .is_none_or(|schedule| { !schedule.trim().is_empty() && schedule.len() <= 100 }),
            "Use the routine ID returned by the routine list."
        );
        ensure!(
            request.limit.is_none_or(|limit| (1..=5).contains(&limit)),
            "Choose between one and five results."
        );
        Ok(request)
    }
}

#[async_trait]
impl Tool for ReadRoutineResults {
    fn name(&self) -> &'static str {
        READ_ROUTINE_RESULTS
    }

    fn describe(&self) -> &'static str {
        "Read the most recent retained Telegram replies from the user's own TinyBot routine, such as daily summaries or scheduled digests. Omit schedule to read the only eligible routine or list choices. Use an ID from the result or list_schedules to select a routine. started_at is Unix time in seconds. Results may be truncated."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "schedule":{"type":"string","description":"Owned routine ID, when known","maxLength":100},
                "limit":{"type":"integer","minimum":1,"maximum":5,"default":3}
            },
            "additionalProperties":false
        })
    }

    async fn inspect(&self, ctx: &ToolContext<'_>, args: &Value) -> Result<Intent> {
        Request::parse(args.clone())?;
        ensure!(
            settings::available(ctx).await?,
            "Routine results are available in the personal Chief chat."
        );
        Ok(Intent {
            risk: Risk::Local,
            destination: String::new(),
            detail: "Read retained results for an owned routine".into(),
        })
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> Result<Outcome> {
        let request = Request::parse(args)?;
        ensure!(
            settings::available(ctx).await?,
            "Routine results are available in the personal Chief chat."
        );
        let result = routine_results::read(
            &ctx.app.db,
            ctx.task,
            request.schedule.as_deref(),
            request.limit.unwrap_or(3),
        )
        .await?;
        ensure!(
            settings::available(ctx).await?,
            "This request changed before the routine results could be read."
        );
        Ok(Outcome::Continue(serde_json::to_value(result)?))
    }
}
