use super::text;
use crate::{App, audit, forms, schedules, store};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::BTreeMap;

pub(super) async fn audit_sessions(app: &App, user: &str, data: &Value) -> Result<Value> {
    audit::sessions(&app.db, user, audit_cursor(data, "before")?).await
}

pub(super) async fn audit_session(app: &App, user: &str, data: &Value) -> Result<Value> {
    let session = audit_cursor(data, "id")?.context("audit session ID required")?;
    let after = audit_cursor(data, "after")?.unwrap_or(0);
    audit::session(&app.db, user, session, after).await
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

pub(super) async fn respond(app: &App, user: &str, data: &Value, approve: bool) -> Result<()> {
    store::respond(&app.db, user, text(data, "id", 64)?, approve).await
}
