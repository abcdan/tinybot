use super::{tasks, text};
use crate::{App, settings::Key, store::settings_requests};
use anyhow::Result;
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
