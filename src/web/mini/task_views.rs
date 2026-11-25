use super::{state, text};
use crate::{
    App,
    store::{self, docblock_saves, task_views},
};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
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
    let mut items = if data["completed_only"] == true {
        task_views::search_in_scope(&app.db, user, query, offset, true).await?
    } else {
        task_views::search(&app.db, user, query, offset).await?
    };
    let next = (items.len() > 20 && offset < 100_000).then_some(offset + 20);
    items.truncate(20);
    Ok(json!({"items":items,"next_offset":next}))
}

pub(super) async fn detail(app: &App, user: &str, data: &Value) -> Result<Value> {
    let task_id = text(data, "task", 64)?;
    let detail = task_views::detail(&app.db, user, task_id)
        .await?
        .context("task unavailable")?;
    let requests = detail
        .requests
        .iter()
        .filter_map(|request| state::request_view(app, user, request))
        .collect::<Vec<_>>();
    let mut view = serde_json::to_value(detail)?;
    view["requests"] = json!(requests);
    view["files"] = json!(store::artifacts::list(&app.db, user, task_id).await?);
    if docblock_saves::available(&app.db, user, task_id).await? && !view["reply"].is_null() {
        view["docblock_saves"] = json!(docblock_saves::list(&app.db, user, task_id).await?);
    }
    Ok(view)
}

pub(super) async fn save_result(app: &App, user: &str, data: &Value) -> Result<Value> {
    let (task, revision) = task_revision(data)?;
    let event = data["event"]
        .as_i64()
        .filter(|id| *id > 0)
        .context("invalid task result")?;
    let source = docblock_saves::Source {
        task,
        revision,
        event,
    };
    ensure!(
        docblock_saves::available(&app.db, user, task).await?,
        "task unavailable"
    );
    let connector = app.mcp.owned(user, text(data, "id", 64)?).await?;
    let workspace = text(data, "workspace", 36)?;
    uuid::Uuid::parse_str(workspace)?;
    let title = text(data, "title", 160)?.trim();
    ensure!(
        !title.is_empty() && !title.chars().any(char::is_control),
        "provide a note title"
    );
    let folder = data["folder"]
        .as_str()
        .filter(|path| path.len() <= 240)
        .context("invalid Docblock folder")?;
    let folders = app.mcp.docblock_folders(&connector, workspace).await?;
    ensure!(
        folder.is_empty() || folders.iter().any(|name| name == folder),
        "The selected Docblock folder is unavailable."
    );
    let destination = docblock_saves::Destination {
        connector: &connector,
        workspace,
        folder,
        title,
    };
    Ok(serde_json::to_value(
        docblock_saves::create(&app.db, user, &source, &destination).await?,
    )?)
}

pub(super) async fn download(app: &App, user: &str, data: &Value) -> Result<Value> {
    let file = store::artifacts::download(
        &app.db,
        &app.vault,
        user,
        text(data, "task", 64)?,
        text(data, "id", 64)?,
        text(data, "kind", 16)?,
    )
    .await?;
    Ok(json!({"name":file.name,"content":STANDARD.encode(file.bytes)}))
}

pub(super) async fn select(app: &App, user: &str, data: &Value) -> Result<()> {
    let selected = if data.get("revision").is_some() {
        let (task_id, revision) = task_revision(data)?;
        task_views::select_at(&app.db, user, task_id, revision).await?
    } else {
        store::select_task(&app.db, user, text(data, "task", 64)?).await?
    };
    ensure!(
        selected,
        "Task changed or is unavailable. Refresh to check its status."
    );
    Ok(())
}

pub(super) async fn start_new(app: &App, user: &str, data: &Value) -> Result<()> {
    let (id, revision) = task_revision(data)?;
    ensure!(
        task_views::start_new(&app.db, user, id, revision).await?,
        "The selected Telegram task changed. Refresh to check its status."
    );
    Ok(())
}

pub(super) async fn stop(app: &App, user: &str, data: &Value) -> Result<()> {
    let (id, revision) = task_revision(data)?;
    ensure!(
        task_views::stop(&app.db, user, id, revision).await?,
        "Task changed or has an action in progress. Refresh to check its status."
    );
    Ok(())
}

pub(super) async fn complete(app: &App, user: &str, data: &Value) -> Result<()> {
    let (id, revision) = task_revision(data)?;
    ensure!(
        task_views::complete(&app.db, user, id, revision).await?,
        "Task changed or has an action in progress. Refresh to check its status."
    );
    Ok(())
}

fn task_revision(data: &Value) -> Result<(&str, i64)> {
    Ok((
        text(data, "task", 64)?,
        data["revision"]
            .as_i64()
            .filter(|revision| *revision >= 0)
            .context("invalid task revision")?,
    ))
}
