use super::{Draft, Event, OPERATION, READ, Range, connection, reply};
use crate::{
    runtime::{Intent, Outcome, Risk, Tool, ToolContext},
    security, store,
};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};

pub(in crate::runtime) const CALENDAR_READ: &str = "calendar_availability";
pub(in crate::runtime) const CALENDAR_DRAFT: &str = "draft_calendar_event";

pub(in crate::runtime) fn definitions() -> [Box<dyn Tool>; 2] {
    [Box::new(Availability), Box::new(DraftEvent)]
}

struct Availability;
struct DraftEvent;

#[async_trait]
impl Tool for Availability {
    fn name(&self) -> &'static str {
        CALENDAR_READ
    }

    fn describe(&self) -> &'static str {
        "Check only the selected calendar's busy windows for exact dated times. An empty busy list means that calendar is free in the requested range."
    }

    fn input_schema(&self) -> Value {
        json!({"type":"object","additionalProperties":false,
            "properties":{"start":{"type":"string","format":"date-time"},"end":{"type":"string","format":"date-time"}},
            "required":["start","end"]})
    }

    async fn inspect(&self, ctx: &ToolContext<'_>, args: &Value) -> Result<Intent> {
        let (_, connector) = connection(ctx).await?;
        let range: Range = serde_json::from_value(args.clone())?;
        range.validate(31)?;
        Ok(Intent {
            risk: Risk::Read,
            destination: connector.name,
            detail: "check the selected calendar's availability".into(),
        })
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> Result<Outcome> {
        let range: Range = serde_json::from_value(args)?;
        range.validate(31)?;
        let (profile, connector) = connection(ctx).await?;
        let result = ctx
            .app
            .mcp
            .call(
                &connector,
                READ,
                &json!({
                    "account":profile.account,"calendars":[{"id":profile.calendar}],
                    "timeMin":range.start,"timeMax":range.end,
                }),
            )
            .await
            .map_err(|_| anyhow::anyhow!("The calendar availability request failed."))?;
        let result = reply(&result).context("The calendar did not return availability.")?;
        ensure!(
            super::same_time(&result["timeMin"], &range.start)
                && super::same_time(&result["timeMax"], &range.end),
            "The calendar did not confirm the requested availability window."
        );
        let calendar = &result["calendars"][&profile.calendar];
        ensure!(
            calendar["errors"].is_null()
                || calendar["errors"].as_array().is_some_and(Vec::is_empty),
            "The selected calendar did not return availability."
        );
        let busy = calendar["busy"]
            .as_array()
            .filter(|busy| busy.len() <= 200)
            .context("No verified busy windows returned. Try a shorter range.")?;
        let mut windows = Vec::with_capacity(busy.len());
        for window in busy {
            let slot = Range {
                start: window["start"]
                    .as_str()
                    .context("Invalid busy window.")?
                    .into(),
                end: window["end"]
                    .as_str()
                    .context("Invalid busy window.")?
                    .into(),
            };
            slot.validate(32)?;
            windows.push(slot);
        }
        Ok(Outcome::Continue(
            json!({"verified":true,"start":range.start,"end":range.end,"busy":windows}),
        ))
    }
}

#[async_trait]
impl Tool for DraftEvent {
    fn name(&self) -> &'static str {
        CALENDAR_DRAFT
    }

    fn describe(&self) -> &'static str {
        "Prepare one dated event on the selected calendar for exact review and approval. Confirm guest names, email addresses and notification choice with the user if unclear. A draft does not create an event."
    }

    fn input_schema(&self) -> Value {
        json!({"type":"object","additionalProperties":false,
            "properties":{
                "title":{"type":"string","maxLength":160},
                "start":{"type":"string","format":"date-time"},
                "end":{"type":"string","format":"date-time"},
                "attendees":{"type":"array","maxItems":12,"items":{
                    "type":"object","additionalProperties":false,
                    "properties":{"name":{"type":"string","maxLength":80},"email":{"type":"string","format":"email"}},
                    "required":["name","email"]
                }},
                "notify_attendees":{"type":"boolean"}
            },
            "required":["title","start","end","attendees","notify_attendees"]
        })
    }

    async fn inspect(&self, ctx: &ToolContext<'_>, args: &Value) -> Result<Intent> {
        let (_, connector) = connection(ctx).await?;
        Event::parse(args)?;
        let already_reviewed: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pending WHERE task=? AND user=? AND revision=?
             AND kind='approve' AND json_extract(payload,'$.calendar_event')=1
             AND state IN ('denied','done','reconciled'))",
        )
        .bind(&ctx.task.id)
        .bind(&ctx.task.user)
        .bind(ctx.task.revision)
        .fetch_one(&ctx.app.db)
        .await?;
        ensure!(
            !already_reviewed,
            "This turn already has a calendar event decision."
        );
        Ok(Intent {
            risk: Risk::Local,
            destination: connector.name,
            detail: OPERATION.into(),
        })
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> Result<Outcome> {
        let event = Event::parse(&args)?;
        let (profile, connector) = connection(ctx).await?;
        let id = security::id();
        let draft = Draft {
            profile,
            connection_label: connector.name,
            event,
        };
        let sealed = ctx.app.vault.seal(
            &Draft::binding(&ctx.task.user, &id),
            &serde_json::to_vec(&draft)?,
        )?;
        store::pause(
            &ctx.app.db,
            ctx.task,
            "approve",
            &json!({"calendar_event":true,"sealed":sealed}),
            "Review the calendar event, guests and invitations in Settings & forms.",
            Some("mini"),
            900,
            &id,
        )
        .await?;
        Ok(Outcome::Pause)
    }
}
