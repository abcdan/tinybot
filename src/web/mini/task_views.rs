use super::text;
use crate::{App, store::task_views};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

pub(super) async fn search(app: &App, user: &str, data: &Value) -> Result<Value> {
    let query = data["query"]
        .as_str()
        .context("invalid task search")?
        .trim();
    ensure!(query.len() <= 800, "task search is too long");
    let offset = match data.get("offset") {
        None => 0,
        Some(value) => value
            .as_i64()
            .filter(|value| (0..=100_000).contains(value))
            .context("invalid task page")?,
    };
    let mut items = task_views::search(&app.db, user, query, offset).await?;
    let next = (items.len() > 20 && offset < 100_000).then_some(offset + 20);
    items.truncate(20);
    Ok(json!({"items":items,"next_offset":next}))
}

pub(super) async fn stop(app: &App, user: &str, data: &Value) -> Result<()> {
    let id = text(data, "task", 64)?;
    let revision = data["revision"]
        .as_i64()
        .filter(|revision| *revision >= 0)
        .context("invalid task revision")?;
    ensure!(
        task_views::stop(&app.db, user, id, revision).await?,
        "Task changed or has an action in progress. Refresh to check its status."
    );
    Ok(())
}
