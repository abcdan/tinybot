mod tools;

use super::ToolContext;
use crate::{App, audit, mcp::Connector, security, store};
use anyhow::{Context, Result, ensure};
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::FromRow;

const READ: &str = "get-freebusy";
const CREATE: &str = "create-event";
const OPERATION: &str = "Create calendar event";

pub(super) use tools::{CALENDAR_DRAFT, CALENDAR_READ, definitions};

#[derive(Clone, Deserialize, FromRow, PartialEq, Serialize)]
struct Selection {
    connector: String,
    approval_epoch: i64,
    version: String,
    account: String,
    calendar: String,
}

async fn selection(app: &App, user: &str) -> Result<Option<Selection>> {
    Ok(sqlx::query_as(
        "SELECT p.connector,p.approval_epoch,p.version,p.account,p.calendar
         FROM calendar_connectors p JOIN connectors c ON c.id=p.connector
         WHERE p.user=? AND c.owner=p.user AND c.auth IN ('api','oauth')
         AND c.approval_epoch=p.approval_epoch",
    )
    .bind(user)
    .fetch_optional(&app.db)
    .await?)
}

pub(crate) async fn settings(app: &App, user: &str, connector: &str) -> Result<Value> {
    Ok(selection(app, user)
        .await?
        .filter(|profile| profile.connector == connector)
        .map_or(
            Value::Null,
            |profile| json!({"account":profile.account,"calendar":profile.calendar}),
        ))
}

pub(crate) async fn configure(
    app: &App,
    user: &str,
    id: &str,
    account: &str,
    calendar: &str,
) -> Result<()> {
    let connector = app.mcp.owned(user, id).await?;
    ensure!(
        matches!(connector.auth.as_str(), "api" | "oauth"),
        "Use an authenticated calendar connection."
    );
    ensure!(
        !account.is_empty()
            && account.len() <= 64
            && account
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)),
        "Enter the calendar connection's account alias."
    );
    ensure!(
        calendar == "primary" || valid_address(calendar),
        "Enter primary or an exact calendar ID."
    );
    let schemas = app
        .mcp
        .list_tools(&connector)
        .await
        .context("Could not verify calendar tools.")?;
    for (tool, fields) in [
        (READ, &["account", "calendars", "timeMin", "timeMax"][..]),
        (
            CREATE,
            &[
                "account",
                "calendarId",
                "summary",
                "start",
                "end",
                "attendees",
                "sendUpdates",
                "eventId",
            ][..],
        ),
    ] {
        ensure!(
            schemas
                .as_array()
                .is_some_and(|items| items.iter().any(|item| {
                    item["name"] == tool
                        && fields
                            .iter()
                            .all(|field| !item["inputSchema"]["properties"][*field].is_null())
                })),
            "This connection does not expose compatible calendar tools."
        );
    }
    let configured = sqlx::query(
        "INSERT INTO calendar_connectors(user,connector,approval_epoch,version,account,calendar)
         SELECT owner,id,approval_epoch,?,?,? FROM connectors
         WHERE id=? AND owner=? AND approval_epoch=? AND auth IN ('api','oauth')
         ON CONFLICT(user) DO UPDATE SET connector=excluded.connector,
         approval_epoch=excluded.approval_epoch,version=excluded.version,
         account=excluded.account,calendar=excluded.calendar",
    )
    .bind(security::id())
    .bind(account)
    .bind(calendar)
    .bind(id)
    .bind(user)
    .bind(connector.approval_epoch)
    .execute(&app.db)
    .await?;
    ensure!(configured.rows_affected() == 1, "The connection changed.");
    Ok(())
}

pub(crate) async fn remove(app: &App, user: &str, connector: &str) -> Result<()> {
    sqlx::query("DELETE FROM calendar_connectors WHERE user=? AND connector=?")
        .bind(user)
        .bind(connector)
        .execute(&app.db)
        .await?;
    Ok(())
}

pub(super) async fn model_context(app: &App, user: &str) -> Result<Option<Value>> {
    Ok(selection(app, user).await?.map(|profile| {
        json!({
            "account":profile.account,
            "calendar_id":profile.calendar,
            "workflow":concat!(
                "Use calendar_availability and draft_calendar_event for this calendar. ",
                "If the user asked for a different calendar, explain which calendar is selected. ",
                "Resolve relative dates using local_time and time_zone. A draft needs explicit approval. ",
                "TinyBot sends its own confirmation after creation. Continue with the remaining work if requested."
            ),
        })
    }))
}

pub(super) async fn require_review_path(
    ctx: &ToolContext<'_>,
    connector: &Connector,
    tool: &str,
) -> Result<()> {
    if tool == CREATE
        && super::settings::available(ctx).await?
        && selection(ctx.app, &ctx.task.user)
            .await?
            .is_some_and(|profile| profile.connector == connector.id)
    {
        anyhow::bail!(
            "Use draft_calendar_event to open an exact review for the selected calendar."
        );
    }
    Ok(())
}

async fn connection(ctx: &ToolContext<'_>) -> Result<(Selection, Connector)> {
    ensure!(
        super::settings::available(ctx).await?,
        "Calendar requests require a private chat."
    );
    let profile = selection(ctx.app, &ctx.task.user)
        .await?
        .context("Choose a calendar in Settings & forms.")?;
    let connector = ctx
        .app
        .mcp
        .owned(&ctx.task.user, &profile.connector)
        .await?;
    ensure!(
        connector.approval_epoch == profile.approval_epoch,
        "The calendar connection changed."
    );
    Ok((profile, connector))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Attendee {
    email: String,
    name: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Range {
    start: String,
    end: String,
}

impl Range {
    fn validate(&self, max_days: i64) -> Result<()> {
        let date_time = |value: &str| {
            DateTime::parse_from_rfc3339(value).context("Provide a full date, time and UTC offset.")
        };
        let start = date_time(&self.start)?;
        let end = date_time(&self.end)?;
        ensure!(
            start < end && end - start <= chrono::Duration::days(max_days),
            "Choose an end after the start within {max_days} days."
        );
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Event {
    title: String,
    start: String,
    end: String,
    attendees: Vec<Attendee>,
    notify_attendees: bool,
}

impl Event {
    fn parse(args: &Value) -> Result<Self> {
        let event: Self = serde_json::from_value(args.clone()).map_err(|_| {
            anyhow::anyhow!("Provide the title, full dates, guests and invitation choice.")
        })?;
        Range {
            start: event.start.clone(),
            end: event.end.clone(),
        }
        .validate(31)?;
        ensure!(short_line(&event.title, 160), "Enter a short event title.");
        ensure!(
            event.attendees.len() <= 12
                && event
                    .attendees
                    .iter()
                    .all(|guest| { valid_address(&guest.email) && short_line(&guest.name, 80) }),
            "Provide a name and address for each guest, with up to 12 guests."
        );
        ensure!(
            !event.notify_attendees || !event.attendees.is_empty(),
            "There are no guests to notify."
        );
        Ok(event)
    }

    fn request(&self, profile: &Selection, id: &str) -> Value {
        let attendees: Vec<_> = self
            .attendees
            .iter()
            .map(|guest| json!({"email":guest.email,"displayName":guest.name}))
            .collect();
        json!({
            "eventId": id,
            "account":profile.account,
            "calendarId":profile.calendar,
            "summary":self.title,
            "start":self.start,
            "end":self.end,
            "attendees":attendees,
            "sendUpdates":if self.notify_attendees {"all"} else {"none"},
        })
    }
}

fn short_line(value: &str, max: usize) -> bool {
    !value.trim().is_empty()
        && value.chars().count() <= max
        && value.chars().all(|c| {
            !c.is_control() && !matches!(c, '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
}

fn valid_address(value: &str) -> bool {
    value.len() <= 254
        && value.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && domain.contains('.')
                && local
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b".+_-%#".contains(&c))
                && domain.split('.').all(|label| {
                    !label.is_empty()
                        && label
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
                })
        })
}

#[derive(Deserialize, Serialize)]
struct Draft {
    profile: Selection,
    connection_label: String,
    event: Event,
}

impl Draft {
    fn binding(user: &str, id: &str) -> String {
        format!("calendar-event:{user}:{id}")
    }

    fn open(app: &App, user: &str, id: &str, payload: &Value) -> Result<Self> {
        let sealed = payload["sealed"]
            .as_str()
            .context("Calendar draft unavailable.")?;
        Ok(serde_json::from_slice(
            &app.vault.open(&Self::binding(user, id), sealed)?,
        )?)
    }
}

pub(crate) fn preview(app: &App, user: &str, id: &str, payload: &Value) -> Result<Value> {
    let draft = Draft::open(app, user, id, payload)?;
    let event = draft.event;
    let mut details = vec![
        format!("Account: {}", draft.profile.account),
        format!("Calendar: {}", draft.profile.calendar),
        format!("Title: {}", event.title),
        format!("Starts: {}", event.start),
        format!("Ends: {}", event.end),
        String::from(if event.attendees.is_empty() {
            "Guests: none"
        } else {
            "Guests:"
        }),
    ];
    details.extend(
        event
            .attendees
            .iter()
            .map(|guest| format!("{} <{}>", guest.name, guest.email)),
    );
    details.push(if event.notify_attendees {
        String::from("Calendar will be asked to send invitations to all listed guests.")
    } else {
        String::from("Calendar will be asked to skip invitation notifications.")
    });
    Ok(
        json!({"operation":OPERATION,"destination":draft.connection_label,"preview":details.join("\n")}),
    )
}

fn reply(response: &Value) -> Option<Value> {
    if response["isError"] == true {
        return None;
    }
    if response["structuredContent"].is_object() {
        return Some(response["structuredContent"].clone());
    }
    let blocks = response["content"].as_array()?;
    if blocks.len() != 1 || blocks[0]["type"] != "text" {
        return None;
    }
    serde_json::from_str(blocks[0]["text"].as_str()?).ok()
}

pub(super) async fn resume(
    ctx: &ToolContext<'_>,
    pending: &store::Pending,
    payload: &Value,
) -> Result<()> {
    let app = ctx.app;
    let task = ctx.task;
    let draft = Draft::open(app, &task.user, &pending.id, payload)?;
    let prepared = connection(ctx).await.ok();
    let Some((profile, connector)) = prepared.filter(|(profile, _)| *profile == draft.profile)
    else {
        return store::finish_action(
            &app.db,
            task,
            &pending.id,
            "Calendar settings changed before creation. Ask me to prepare a new event for review.",
            true,
        )
        .await;
    };
    if !ctx.still_allows(CALENDAR_DRAFT).await? {
        return store::finish_action(
            &app.db,
            task,
            &pending.id,
            "This calendar approval no longer applies to the current task.",
            false,
        )
        .await;
    }
    let event_id = pending.id.replace('-', "");
    audit::record("tool", "calendar_event", "approved", Value::Null).await;
    let sent = app
        .mcp
        .call(
            &connector,
            CREATE,
            &draft.event.request(&profile, &event_id),
        )
        .await;
    let saved = sent.ok().and_then(|result| reply(&result));
    let verified = saved.as_ref().is_some_and(|result| {
        let event = &result["event"];
        event["id"] == event_id
            && event["summary"] == draft.event.title
            && event["accountId"] == profile.account
            && (event["calendarId"] == profile.calendar
                || (profile.calendar == "primary"
                    && event["calendarId"].as_str().is_some_and(valid_address)))
            && same_time(&event["start"]["dateTime"], &draft.event.start)
            && same_time(&event["end"]["dateTime"], &draft.event.end)
            && draft.event.attendees.iter().all(|guest| {
                event["attendees"].as_array().is_some_and(|guests| {
                    guests.iter().any(|entry| {
                        entry["email"]
                            .as_str()
                            .is_some_and(|email| email.eq_ignore_ascii_case(&guest.email))
                    })
                })
            })
    });
    if verified {
        audit::record("tool", "calendar_event", "verified", Value::Null).await;
        return store::finish_action(
            &app.db,
            task,
            &pending.id,
            "Calendar confirmed the event was added.",
            true,
        )
        .await;
    }
    audit::record("tool", "calendar_event", "uncertain", Value::Null).await;
    store::uncertain_action(&app.db, task, &pending.id,
        "Calendar creation could not be verified. Check the calendar for the event before sending /reconcile.").await
}

fn same_time(value: &Value, expected: &str) -> bool {
    let actual = value
        .as_str()
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok());
    actual.is_some() && actual == DateTime::parse_from_rfc3339(expected).ok()
}
