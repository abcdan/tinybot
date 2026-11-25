use super::{tasks, text};
use crate::{
    App,
    settings::Key,
    store::{settings_history, settings_requests},
};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;

pub(super) async fn get_form(app: &App, user: &str, data: &Value) -> Result<Value> {
    let id = text(data, "id", 36)?;
    if let Some(form) = settings_requests::get(&app.db, user, id).await? {
        return Ok(form);
    }
    tasks::get_form(app, user, data).await
}

pub(super) async fn answer_form(app: &App, user: &str, data: &Value) -> Result<Value> {
    let request = text(data, "id", 36)?;
    let values: BTreeMap<Key, String> = serde_json::from_value(data["values"].clone())?;
    settings_requests::answer(app, user, request, values).await
}

pub(super) async fn approve(app: &App, user: &str, data: &Value) -> Result<()> {
    settings_requests::approve(app, user, text(data, "id", 36)?).await
}

pub(super) async fn history(app: &App, user: &str, data: &Value) -> Result<Value> {
    let before = data
        .get("before")
        .map(|value| {
            value
                .as_i64()
                .filter(|id| *id > 0)
                .context("invalid history page")
        })
        .transpose()?;
    settings_history::list(&app.db, user, before).await
}

pub(super) async fn propose_restore(app: &App, user: &str, data: &Value) -> Result<Value> {
    let id = data["id"]
        .as_i64()
        .filter(|id| *id > 0)
        .context("invalid settings change")?;
    settings_requests::propose_restore(app, user, id).await
}
