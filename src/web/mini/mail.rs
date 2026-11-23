use crate::{App, store::mail};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Deserialize)]
struct AccountUpdate {
    id: Option<String>,
    config: mail::MailConfig,
}

pub(super) async fn save(app: &App, user: &str, data: &Value) -> Result<Value> {
    let AccountUpdate { id, config } =
        serde_json::from_value(data.clone()).context("invalid email account settings")?;
    if let Some(id) = &id {
        Uuid::parse_str(id).context("invalid email account ID")?;
    }
    let id = mail::save_account(&app.db, &app.vault, user, id.as_deref(), config).await?;
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
