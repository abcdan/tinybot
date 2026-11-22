use super::{Chat, User};
use crate::{forms, schedules, security::clip, store};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use std::time::Duration;

const PAGE_SIZE: usize = 2;

#[derive(Deserialize)]
pub struct Query {
    pub id: String,
    from: User,
    chat: Option<Chat>,
    message_id: Option<i64>,
    #[serde(rename = "type")]
    kind: Value,
    data: String,
}

impl Query {
    pub(super) fn is_private(&self) -> bool {
        self.chat
            .as_ref()
            .is_some_and(|chat| chat.is_private_with(&self.from))
            && self.message_id.is_some()
            && self.kind["type"] == "data"
    }
}

#[derive(Serialize)]
struct Row {
    id: String,
    title: String,
    detail: String,
    actionable: bool,
    choice: bool,
}

impl Row {
    fn new(id: String, title: &str, detail: &str, actionable: bool) -> Self {
        Self {
            id,
            title: display(title, 88),
            detail: display(detail, 70),
            actionable,
            choice: false,
        }
    }
}

fn display(value: &str, limit: usize) -> String {
    let value: String = value.chars().filter(|ch| !ch.is_control()).collect();
    let clipped = clip(&value, limit);
    if clipped.len() == value.len() {
        value
    } else {
        format!("{clipped}…")
    }
}

#[derive(Serialize)]
struct Page {
    view: &'static str,
    page: usize,
    more: bool,
    notice: &'static str,
    rows: Vec<Row>,
}

pub(super) async fn answer(db: &SqlitePool, query: &Query) -> String {
    let result = tokio::time::timeout(
        Duration::from_secs(8),
        query_reply(db, &query.from.id.to_string(), &query.data),
    )
    .await;
    if let Ok(Ok(page)) = result
        && let Ok(answer) = serde_json::to_string(&page)
        && answer.len() <= 1024
    {
        return answer;
    }
    r#"{"error":"This request is unavailable. Refresh the panel to try again."}"#.into()
}

async fn query_reply(db: &SqlitePool, user: &str, data: &str) -> Result<Value> {
    ensure!(data.len() <= 64, "oversized widget query");
    if let Some(request) = data.strip_prefix("choice/") {
        uuid::Uuid::parse_str(request)?;
        return Ok(json!({"choice":forms::widget_choice(db, user, request).await?}));
    }
    if let Some(selection) = data.strip_prefix("answer/") {
        let (request, option) = selection.split_once('/').context("missing answer")?;
        uuid::Uuid::parse_str(request)?;
        forms::answer_choice(db, user, request, option.parse()?).await?;
        let mut page = panel(db, user, "requests/0").await?;
        page.notice = "Answer sent.";
        return Ok(serde_json::to_value(page)?);
    }
    Ok(serde_json::to_value(panel(db, user, data).await?)?)
}

async fn panel(db: &SqlitePool, user: &str, data: &str) -> Result<Page> {
    let (action, value) = data.split_once('/').context("invalid widget query")?;
    let (provider, active): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT provider,active_task FROM users WHERE id=?")
            .bind(user)
            .fetch_one(db)
            .await?;
    let (view, page, notice) = match action {
        "requests" | "tasks" | "schedules" => {
            let page = value.parse::<usize>()?;
            ensure!(page < 30, "invalid page");
            let notice = if provider.is_none() {
                "Connect a model in Settings & forms."
            } else {
                ""
            };
            (action, page, notice)
        }
        "select" => {
            ensure!(
                store::select_task(db, user, value).await?,
                "task unavailable"
            );
            return task_page(
                db,
                user,
                Some(value),
                0,
                "Task selected for your next message.",
            )
            .await;
        }
        "decline" => {
            store::respond(db, user, value, false).await?;
            ("requests", 0, "Request declined.")
        }
        "cancel" => {
            schedules::cancel(db, user, value).await?;
            ("schedules", 0, "Schedule cancelled.")
        }
        _ => anyhow::bail!("unknown widget action"),
    };
    match view {
        "tasks" => task_page(db, user, active.as_deref(), page, notice).await,
        "schedules" => {
            let rows = schedules::list(db, user)
                .await?
                .into_iter()
                .map(|schedule| {
                    let at = schedule.next_run.and_then(|date| {
                        let time = chrono::DateTime::from_timestamp(date, 0)?;
                        let zone = schedule.time_zone.parse::<chrono_tz::Tz>().ok()?;
                        Some(
                            time.with_timezone(&zone)
                                .format("%d %b %Y, %H:%M %Z")
                                .to_string(),
                        )
                    });
                    Row::new(
                        schedule.id,
                        &schedule.title,
                        at.as_deref().unwrap_or(&schedule.state),
                        schedule.state == "active",
                    )
                });
            Ok(page_of("schedules", page, notice, rows))
        }
        _ => {
            let rows = store::list_requests(db, user)
                .await?
                .into_iter()
                .map(|request| {
                    let label = match request.kind.as_str() {
                        "agent-spec" => "Configure an agent",
                        "form" => "Enter form details",
                        "form-submit" => "Review website submission",
                        "elicitation" => "Answer questions",
                        _ => "Review an action",
                    };
                    let mut row =
                        Row::new(request.id, label, "Open Settings & forms to review.", true);
                    if request.kind == "elicitation"
                        && let Ok(spec) = serde_json::from_str::<forms::FormSpec>(&request.payload)
                        && spec.validate().is_ok()
                        && let Some(field) = spec.widget_choice()
                    {
                        row.title = display(&field.label, 88);
                        row.detail.clear();
                        row.choice = true;
                    }
                    row
                });
            Ok(page_of("requests", page, notice, rows))
        }
    }
}

async fn task_page(
    db: &SqlitePool,
    user: &str,
    active: Option<&str>,
    page: usize,
    notice: &'static str,
) -> Result<Page> {
    let rows = store::list_tasks(db, user).await?.into_iter().map(|task| {
        let selected = active == Some(task.id.as_str());
        let detail = if selected {
            "Active in chat"
        } else {
            &task.state
        };
        Row::new(
            task.id,
            &task.title,
            detail,
            !selected && task.state != "cancelled",
        )
    });
    Ok(page_of("tasks", page, notice, rows))
}

fn page_of(
    view: &'static str,
    page: usize,
    notice: &'static str,
    rows: impl Iterator<Item = Row>,
) -> Page {
    let mut rows: Vec<_> = rows.skip(page * PAGE_SIZE).take(PAGE_SIZE + 1).collect();
    let more = rows.len() > PAGE_SIZE;
    rows.truncate(PAGE_SIZE);
    Page {
        view,
        page,
        more,
        notice,
        rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sqlx::sqlite::SqlitePoolOptions;

    fn query(chat: i64, kind: &str, data: &str) -> Query {
        serde_json::from_value(json!({
            "id": "widget-query",
            "from": {"id": 1, "is_bot": false},
            "chat": {"id": chat, "type": "private"},
            "message_id": 42,
            "type": {"type": kind},
            "data": data
        }))
        .unwrap()
    }

    #[test]
    fn widget_queries_are_private_data_updates() {
        assert!(query(1, "data", "tasks/0").is_private());
        assert!(!query(2, "data", "tasks/0").is_private());
        assert!(!query(1, "sign_data", "tasks/0").is_private());
    }

    #[tokio::test]
    async fn widget_only_reads_and_changes_the_callers_tasks_and_schedules() {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&db).await.unwrap();
        for id in ["1", "2"] {
            store::seed(&db, id).await.unwrap();
        }
        let quoted = "\"".repeat(100);
        for (id, user, title) in [
            ("first", "1", quoted.as_str()),
            ("second", "1", quoted.as_str()),
            ("other", "2", "private task"),
        ] {
            sqlx::query("INSERT INTO tasks(id,user,title,updated) VALUES(?,?,?,1)")
                .bind(id)
                .bind(user)
                .bind(title)
                .execute(&db)
                .await
                .unwrap();
        }
        let data = answer(&db, &query(1, "data", "tasks/0")).await;
        assert!(data.len() <= 1024);
        let response: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(response["rows"].as_array().unwrap().len(), 2);
        assert!(panel(&db, "1", "select/other").await.is_err());
        let selected = panel(&db, "1", "select/second").await.unwrap();
        assert!(
            !selected
                .rows
                .iter()
                .find(|row| row.id == "second")
                .unwrap()
                .actionable
        );
        sqlx::query(
            "INSERT INTO schedules(id,owner,title,input,kind,time_zone,next_run)
            VALUES('private','2','Private schedule','private prompt','task','UTC',1)",
        )
        .execute(&db)
        .await
        .unwrap();
        assert!(
            panel(&db, "1", "schedules/0")
                .await
                .unwrap()
                .rows
                .is_empty()
        );
        assert!(panel(&db, "1", "cancel/private").await.is_err());
        assert!(!panel(&db, "2", "cancel/private").await.unwrap().rows[0].actionable);
    }
}
