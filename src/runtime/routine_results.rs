use super::{Intent, Outcome, Risk, Tool, ToolContext, settings};
use crate::schedules::routine_results;
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use chrono::NaiveDate;
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
    title: Option<String>,
    date: Option<String>,
    limit: Option<i64>,
}

impl Request {
    fn parse(args: Value) -> Result<Request> {
        let mut request: Self = serde_json::from_value(args)?;
        request.title = request.title.map(|title| title.trim().to_owned());
        ensure!(
            request
                .schedule
                .as_ref()
                .is_none_or(|schedule| { !schedule.trim().is_empty() && schedule.len() <= 100 }),
            "Use the routine ID returned by the routine list."
        );
        ensure!(
            request
                .title
                .as_ref()
                .is_none_or(|title| !title.is_empty() && title.len() <= 100),
            "Use the complete routine title."
        );
        ensure!(
            request.title.is_none() || request.schedule.is_none(),
            "Use either the routine title or ID."
        );
        ensure!(
            request.limit.is_none_or(|limit| (1..=5).contains(&limit)),
            "Choose between one and five results."
        );
        request.date()?;
        Ok(request)
    }

    fn date(&self) -> Result<Option<NaiveDate>> {
        self.date
            .as_deref()
            .map(|date| {
                let parsed = NaiveDate::parse_from_str(date, "%Y-%m-%d")
                    .context("Use a valid date in YYYY-MM-DD format.")?;
                ensure!(
                    parsed.to_string() == date && date.len() == 10,
                    "Use YYYY-MM-DD for the date."
                );
                Ok(parsed)
            })
            .transpose()
    }
}

#[async_trait]
impl Tool for ReadRoutineResults {
    fn name(&self) -> &'static str {
        READ_ROUTINE_RESULTS
    }

    fn describe(&self) -> &'static str {
        "Read retained Telegram replies from the user's own TinyBot routines. Use the known complete title or select a routine by ID. Omit both to read the only eligible routine or list choices. Use date (YYYY-MM-DD) for a specific day in the routine's time zone. Results may be truncated."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "schedule":{"type":"string","description":"Owned routine ID, when known","maxLength":100},
                "title":{"type":"string","description":"Complete routine title when its ID is unknown","maxLength":100},
                "date":{"type":"string","description":"Run date in the routine time zone, YYYY-MM-DD","format":"date"},
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
            routine_results::Query {
                schedule: request.schedule.as_deref(),
                title: request.title.as_deref(),
                date: request.date()?,
                limit: request.limit.unwrap_or(3),
            },
        )
        .await?;
        ensure!(
            settings::available(ctx).await?,
            "This request changed before the routine results could be read."
        );
        Ok(Outcome::Continue(serde_json::to_value(result)?))
    }
}
