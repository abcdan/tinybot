use crate::{App, store::mail};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Deserialize)]
struct AccountUpdate {
    id: Option<String>,
    config: mail::MailConfig,
    request: Option<String>,
}

pub(super) async fn save(app: &App, user: &str, data: &Value) -> Result<Value> {
    let AccountUpdate {
        id,
        config,
        request,
    } = serde_json::from_value(data.clone()).context("invalid email account settings")?;
    if let Some(id) = &id {
        Uuid::parse_str(id).context("invalid email account ID")?;
    }
    let id = mail::save_for_task(
        &app.db,
        &app.vault,
        user,
        id.as_deref(),
        config,
        request.as_deref(),
    )
    .await?;
    Ok(json!({"id":id}))
}

pub(super) async fn remove(app: &App, user: &str, data: &Value) -> Result<()> {
    let id = super::text(data, "id", 36)?;
    Uuid::parse_str(id).context("invalid email account ID")?;
    ensure!(
        mail::remove_account(&app.db, user, id).await?,
        "email account is no longer available"
    );
    Ok(())
}

pub(super) async fn edit_draft(app: &App, user: &str, data: &Value) -> Result<Value> {
    let id = super::text(data, "id", 36)?;
    Uuid::parse_str(id).context("invalid email draft ID")?;
    let edit = serde_json::from_value(data["draft"].clone()).context("invalid email draft")?;
    Ok(json!({"id":crate::runtime::edit_draft(app,user,id,edit).await?}))
}
