use super::text;
use crate::{App, store::projects};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub(super) async fn operation(app: &App, user: &str, action: &str, data: &Value) -> Result<Value> {
    let db = &app.db;
    match action {
        "list-projects" => return Ok(json!({"projects": projects::list(db, user).await?})),
        "create-project" => {
            let project = projects::create(db, user, text(data, "title", 120)?).await?;
            return Ok(json!({"project":project}));
        }
        _ => {}
    }

    let project = text(data, "project", 64)?;
    match action {
        "project-detail" => {
            let offset = data["offset"].as_i64().unwrap_or(0).clamp(0, 100_000);
            Ok(serde_json::to_value(
                projects::detail(db, user, project, offset).await?,
            )?)
        }
        "project-member" => {
            let change = serde_json::from_value(data["change"].clone())?;
            projects::update_member(db, user, project, change).await?;
            Ok(json!({"ok":true}))
        }
        "preview-project-result" => {
            let task = text(data, "task", 64)?;
            Ok(serde_json::to_value(
                projects::task_result(db, user, project, task).await?,
            )?)
        }
        "publish-project-item" => {
            let review = serde_json::from_value(data["review"].clone())?;
            let source = serde_json::from_value(data["source"].clone())?;
            let item = projects::publish(db, user, project, review, source).await?;
            Ok(json!({"item":item}))
        }
        "project-item" => {
            let item = text(data, "item", 64)?;
            let revision = data["revision"].as_i64().context("invalid item revision")?;
            let change = serde_json::from_value(data["change"].clone())?;
            projects::update_item(db, user, project, item, revision, change).await?;
            Ok(json!({"ok":true}))
        }
        _ => bail!("unknown project action"),
    }
}
