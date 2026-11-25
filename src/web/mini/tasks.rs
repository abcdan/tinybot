use super::text;
use crate::{App, audit, forms, schedules, security::now, store};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(super) async fn audit_sessions(app: &App, user: &str, data: &Value) -> Result<Value> {
    audit::sessions(&app.db, user, audit_cursor(data, "before")?).await
}

pub(super) async fn audit_session(app: &App, user: &str, data: &Value) -> Result<Value> {
    let session = audit_cursor(data, "id")?.context("audit session ID required")?;
    let after = audit_cursor(data, "after")?.unwrap_or(0);
    audit::session(&app.db, user, session, after).await
}

pub(super) async fn audit_export(app: &App, user: &str, data: &Value) -> Result<Value> {
    let session = audit_cursor(data, "id")?.context("audit session ID required")?;
    audit::export(&app.db, user, session).await
}

fn audit_cursor(data: &Value, field: &str) -> Result<Option<i64>> {
    data.get(field)
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_i64()
                .filter(|id| *id > 0)
                .context("invalid audit cursor")
        })
        .transpose()
}

pub(super) async fn select_task(app: &App, user: &str, data: &Value) -> Result<()> {
    let task = text(data, "task", 64)?;
    ensure!(
        store::select_task(&app.db, user, task).await?,
        "task unavailable"
    );
    Ok(())
}

pub(super) async fn get_form(app: &App, user: &str, data: &Value) -> Result<Value> {
    forms::input_request(&app.db, user, text(data, "id", 36)?).await
}

pub(super) async fn submit_form(app: &App, user: &str, data: &Value) -> Result<()> {
    let request = text(data, "id", 64)?;
    let values: BTreeMap<String, String> = serde_json::from_value(data["values"].clone())?;
    app.forms.receive(&app.db, user, request, values).await
}

pub(super) async fn answer_form(app: &App, user: &str, data: &Value) -> Result<()> {
    let request = text(data, "id", 64)?;
    let values: BTreeMap<String, String> = serde_json::from_value(data["values"].clone())?;
    forms::answer(&app.db, user, request, values).await
}

pub(super) async fn set_time_zone(app: &App, user: &str, data: &Value) -> Result<()> {
    schedules::set_zone(&app.db, user, text(data, "zone", 100)?).await
}

pub(super) async fn cancel_schedule(app: &App, user: &str, data: &Value) -> Result<()> {
    schedules::cancel(&app.db, user, text(data, "id", 64)?).await
}

pub(super) async fn schedule_details(app: &App, user: &str, data: &Value) -> Result<Value> {
    let details = schedules::details(&app.db, user, text(data, "id", 64)?).await?;
    Ok(serde_json::to_value(details)?)
}

pub(super) async fn create_routine(app: &App, user: &str, data: &Value) -> Result<()> {
    let source = data["source"].as_str();
    ensure!(
        source.is_none() || source == Some("inbox"),
        "unknown routine source"
    );
    schedules::save_routine(&app.db, user, routine_request(data)?, source, now(), None).await?;
    Ok(())
}

pub(super) async fn edit_schedule(app: &App, user: &str, data: &Value) -> Result<()> {
    schedules::edit(
        &app.db,
        user,
        text(data, "id", 64)?,
        revision(data)?,
        routine_request(data)?,
        now(),
    )
    .await
}

pub(super) async fn pause_schedule(app: &App, user: &str, data: &Value, pause: bool) -> Result<()> {
    schedules::set_paused(
        &app.db,
        user,
        text(data, "id", 64)?,
        revision(data)?,
        pause,
        now(),
    )
    .await
}

pub(super) async fn run_schedule(app: &App, user: &str, data: &Value) -> Result<Value> {
    let task = schedules::run_now(
        &app.db,
        user,
        text(data, "id", 64)?,
        revision(data)?,
        run_request(data)?,
        now(),
        None,
    )
    .await?;
    Ok(json!({"task":task}))
}

pub(super) async fn preview_routine(app: &App, user: &str, data: &Value) -> Result<Value> {
    let task = schedules::queue_preview(
        &app.db,
        user,
        text(data, "id", 64)?,
        revision(data)?,
        run_request(data)?,
        text(data, "sample", 2_000)?,
        now(),
    )
    .await?;
    Ok(json!({"task":task}))
}

fn run_request(data: &Value) -> Result<&str> {
    let request = text(data, "request_id", 36)?;
    ensure!(
        uuid::Uuid::parse_str(request).is_ok(),
        "invalid run request"
    );
    Ok(request)
}

pub(super) async fn schedule_history(app: &App, user: &str, data: &Value) -> Result<Value> {
    let id = text(data, "id", 64)?;
    let runs = schedules::history(&app.db, user, id).await?;
    let occurrences = schedules::occurrences(&app.db, user, id).await?;
    Ok(json!({"runs":runs,"occurrences":occurrences}))
}

pub(super) async fn schedule_policy(app: &App, user: &str, data: &Value) -> Result<()> {
    let policy: schedules::Policy =
        serde_json::from_value(data["policy"].clone()).context("invalid schedule policy")?;
    schedules::set_policy(
        &app.db,
        user,
        text(data, "id", 64)?,
        revision(data)?,
        policy,
    )
    .await
}

pub(super) async fn snooze_schedule(app: &App, user: &str, data: &Value) -> Result<()> {
    schedules::snooze(
        &app.db,
        user,
        text(data, "id", 64)?,
        revision(data)?,
        data["minutes"]
            .as_i64()
            .context("invalid snooze duration")?,
        now(),
    )
    .await
}

fn revision(data: &Value) -> Result<i64> {
    data["revision"]
        .as_i64()
        .filter(|revision| *revision >= 0)
        .context("invalid schedule revision")
}

fn routine_request(data: &Value) -> Result<schedules::RoutineRequest> {
    serde_json::from_value(data["routine"].clone()).context("invalid routine")
}

pub(super) async fn respond(app: &App, user: &str, data: &Value, approve: bool) -> Result<()> {
    store::respond(&app.db, user, text(data, "id", 64)?, approve).await
}

pub(super) async fn always_approve_mcp(app: &App, user: &str, data: &Value) -> Result<()> {
    store::respond_mcp(
        &app.db,
        user,
        text(data, "id", 64)?,
        store::McpApprovalDecision::Always,
    )
    .await
}
