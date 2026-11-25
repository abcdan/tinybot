use super::{app_with_model, fixture, message, pending_id};
use crate::{
    model::{ModelProvider, TextDelta, ToolDefinition},
    runtime, schedules,
    security::now,
    store,
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct RoutineChief;

#[async_trait]
impl ModelProvider for RoutineChief {
    async fn complete(&self, system: &str, input: &str) -> Result<String> {
        if system.starts_with("Review this proposed TinyBot tool action") {
            return Ok(
                json!({"verdict":"allow","reason":"The user requested a routine."}).to_string(),
            );
        }
        let context: Value = serde_json::from_str(input)?;
        let result = if system.starts_with("Write one short, casual chat sentence") {
            json!({"text":"Review will check the draft."})
        } else if system.starts_with("You are a temporary helper") {
            assert!(context["brief"].as_str().unwrap().contains("First result"));
            json!({"kind":"reply","text":"Helper result"})
        } else if let Some(routine) = context.get("routine") {
            let results = routine["results"].as_array().unwrap();
            match results.len() {
                0 => {
                    assert_eq!(routine["current"]["instruction"], "Draft a summary.");
                    json!({"kind":"tool","name":"record_routine_step","args":{"index":0,"result":"First result"}})
                }
                1 => {
                    assert_eq!(routine["current"]["agent"], "routine-helper");
                    json!({"kind":"tool","name":"record_routine_step","args":{"index":1}})
                }
                2 => {
                    assert_eq!(results[1], "Helper result");
                    json!({"kind":"reply","text":"The routine finished."})
                }
                _ => unreachable!(),
            }
        } else if context["history"].as_array().unwrap().iter().any(|event| {
            event["role"] == "tool" && event["text"].as_str().unwrap().contains("\"revision\":0")
        }) {
            json!({"kind":"reply","text":"I saved the routine."})
        } else {
            json!({"kind":"tool","name":"save_routine","args":{"title":"Weekly brief","input":"Summarize and review.",
                "steps":[{"instruction":"Draft a summary."},{"instruction":"Check the draft.","agent":"routine-helper"}]}})
        };
        Ok(result.to_string())
    }
}

#[tokio::test]
async fn a_chat_routine_requires_approval_and_keeps_chief_and_helper_steps_in_order() {
    let db = fixture().await;
    sqlx::query("INSERT INTO agents (id,owner,name,instructions,tools) VALUES('routine-helper','1','Review','Review the draft','[]')")
        .execute(&db).await.unwrap();
    let app = app_with_model(&db, Arc::new(RoutineChief));
    message(&db, 1, "1", "Save this review process as a routine.").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    assert!(schedules::list(&db, "1").await.unwrap().is_empty());
    let approval = pending_id(&db, &task, "approve").await;
    store::respond(&db, "1", &approval, true).await.unwrap();
    let task = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    let routine = schedules::list(&db, "1").await.unwrap().pop().unwrap();
    assert_eq!(routine.state, "manual");
    let run = schedules::run_now(&db, "1", &routine.id, routine.revision, "run", now(), None)
        .await
        .unwrap();
    let task = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(run, task.id);
    runtime::work(&app, &task).await.unwrap();
    let result: String = sqlx::query_scalar("SELECT results FROM schedule_runs WHERE task=?")
        .bind(&run)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(result, r#"["First result","Helper result"]"#);
    let reply: String =
        sqlx::query_scalar("SELECT body FROM outbox WHERE task=? ORDER BY id DESC LIMIT 1")
            .bind(&run)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(reply, "The routine finished.");
}

struct PreviewModel;

#[async_trait]
impl ModelProvider for PreviewModel {
    async fn complete(&self, _: &str, _: &str) -> Result<String> {
        unreachable!()
    }

    async fn complete_turn(
        &self,
        instructions: &str,
        input: &str,
        tools: &[ToolDefinition],
        _: Option<&mut TextDelta<'_>>,
    ) -> Result<String> {
        assert!(tools.is_empty());
        assert!(instructions.contains("Never claim any tool"));
        let input: Value = serde_json::from_str(input)?;
        assert_eq!(
            input,
            json!({"instructions":"Review meeting notes.",
            "sample":"Sample decision: ship on Friday.","steps":["Draft a private note."]})
        );
        Ok(json!({"text":"I would draft a note with the Friday decision."}).to_string())
    }
}

#[tokio::test]
async fn a_routine_dry_run_simulates_saved_steps_without_running_tools() {
    let db = fixture().await;
    let app = app_with_model(&db, Arc::new(PreviewModel));
    let time = now();
    let schedule = schedules::save_routine(
        &db,
        "1",
        schedules::RoutineRequest {
            title: "Meeting notes".into(),
            input: "Review meeting notes.".into(),
            at: None,
            cron: None,
            steps: vec![schedules::RoutineStep {
                instruction: "Draft a private note.".into(),
                agent: None,
            }],
        },
        None,
        time,
        None,
    )
    .await
    .unwrap();
    assert!(
        schedules::queue_preview(&db, "2", &schedule.id, 0, "foreign", "sample", time)
            .await
            .is_err()
    );
    let sample = "Sample decision: ship on Friday.";
    let preview = schedules::queue_preview(&db, "1", &schedule.id, 0, "fixture", sample, time)
        .await
        .unwrap();
    assert_eq!(
        schedules::queue_preview(&db, "1", &schedule.id, 0, "fixture", sample, time)
            .await
            .unwrap(),
        preview
    );
    let task = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(task.id, preview);
    let live = schedules::run_now(&db, "1", &schedule.id, 0, "live", time, None)
        .await
        .unwrap();
    assert_ne!(live, preview);
    runtime::work(&app, &task).await.unwrap();
    assert_eq!(
        store::claim_outbox(&db).await.unwrap().unwrap().body,
        "Dry run\n\nI would draft a note with the Friday decision."
    );
    let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM task_tool_attempts WHERE task=?")
        .bind(&preview)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(attempts, 0);
    assert!(
        store::list_tasks(&db, "1")
            .await
            .unwrap()
            .iter()
            .all(|item| item.id != preview)
    );
}

#[tokio::test]
async fn a_signed_webhook_queues_one_run_and_cannot_replay_after_revision_change() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use tower_service::Service;

    let db = fixture().await;
    let app = Arc::new(app_with_model(&db, Arc::new(PreviewModel)));
    let time = now();
    let routine = schedules::save_routine(
        &db,
        "1",
        schedules::RoutineRequest {
            title: "Inbound briefing".into(),
            input: "Draft a briefing.".into(),
            at: None,
            cron: Some("0 9 * * *".into()),
            steps: vec![schedules::RoutineStep {
                instruction: "Read the latest status.".into(),
                agent: None,
            }],
        },
        None,
        time,
        None,
    )
    .await
    .unwrap();
    assert!(
        schedules::webhooks::install(&db, &app.vault, "2", &routine.id, 0, time)
            .await
            .is_err()
    );
    let hook = schedules::webhooks::install(&db, &app.vault, "1", &routine.id, 0, time)
        .await
        .unwrap();
    let body = r#"{"event_id":"fixture-first"}"#;
    let request = |body: &str, at: i64, signed: bool| {
        let timestamp = at.to_string();
        let mut mac = Hmac::<Sha256>::new_from_slice(hook.secret.as_bytes()).unwrap();
        mac.update(timestamp.as_bytes());
        mac.update(b".");
        mac.update(body.as_bytes());
        let signature = if signed {
            hex::encode(mac.finalize().into_bytes())
        } else {
            "0".repeat(64)
        };
        Request::builder()
            .method("POST")
            .uri(format!("/hooks/routine/{}", hook.id))
            .header("x-tinybot-timestamp", timestamp)
            .header("x-tinybot-signature", signature)
            .body(Body::from(body.to_owned()))
            .unwrap()
    };
    let mut router = crate::web::router(app);
    for (event, at, signed, expected) in [
        (body, time, false, StatusCode::UNAUTHORIZED),
        (body, time - 301, true, StatusCode::UNAUTHORIZED),
        (body, time, true, StatusCode::ACCEPTED),
        (body, time, true, StatusCode::ACCEPTED),
        (
            r#"{"event_id":"fixture-second"}"#,
            time,
            true,
            StatusCode::CONFLICT,
        ),
    ] {
        assert_eq!(
            router
                .clone()
                .call(request(event, at, signed))
                .await
                .unwrap()
                .status(),
            expected
        );
    }
    let runs =
        serde_json::to_value(schedules::history(&db, "1", &routine.id).await.unwrap()).unwrap();
    assert_eq!(runs.as_array().unwrap().len(), 1);
    assert_eq!(runs[0]["trigger"], "webhook");
    schedules::set_paused(&db, "1", &routine.id, 0, true, time)
        .await
        .unwrap();
    assert_eq!(
        router
            .call(request(body, time, true))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

struct ChangeModel(AtomicUsize);

#[async_trait]
impl ModelProvider for ChangeModel {
    async fn complete(&self, instructions: &str, input: &str) -> Result<String> {
        let data: Value = serde_json::from_str(input)?;
        if instructions.starts_with("Compare these two results") {
            assert_eq!(data["previous"], "The release is Friday.");
            let changed = data["current"].as_str().unwrap().contains("Monday");
            return Ok(json!({"changed":changed}).to_string());
        }
        if data["routine"]["current"].is_object() {
            return Ok(json!({"kind":"tool","name":"record_routine_step","args":{"index":0,"result":"Confirmed source"}}).to_string());
        }
        let replies = [
            "The release is Friday.",
            "Friday is the release date.",
            "The release is Monday.",
        ];
        let text = replies[self.0.fetch_add(1, Ordering::SeqCst)];
        Ok(json!({"kind":"reply","text":text}).to_string())
    }
}

#[tokio::test]
async fn a_change_only_routine_compares_against_the_last_delivered_result() {
    let db = fixture().await;
    let app = app_with_model(&db, Arc::new(ChangeModel(AtomicUsize::new(0))));
    let time = now();
    let schedule = schedules::save_routine(
        &db,
        "1",
        schedules::RoutineRequest {
            title: "Release watch".into(),
            input: "Report the release date.".into(),
            at: None,
            cron: Some("*/5 * * * *".into()),
            steps: vec![schedules::RoutineStep {
                instruction: "Review the source.".into(),
                agent: None,
            }],
        },
        None,
        time,
        None,
    )
    .await
    .unwrap();
    let policy: schedules::Policy = serde_json::from_value(json!({
        "catch_up":"coalesce","notify_changes":true,
    }))
    .unwrap();
    schedules::set_policy(&db, "1", &schedule.id, 0, policy)
        .await
        .unwrap();

    for (index, expected) in [
        Some("The release is Friday."),
        None,
        Some("The release is Monday."),
    ]
    .into_iter()
    .enumerate()
    {
        let due = schedule.next_run.unwrap() + i64::try_from(index).unwrap() * 300;
        schedules::dispatch(&db, due).await.unwrap();
        let task = store::claim(&db).await.unwrap().unwrap();
        runtime::work(&app, &task).await.unwrap();
        let sent = store::claim_outbox(&db).await.unwrap();
        assert_eq!(sent.as_ref().map(|item| item.body.as_str()), expected);
        if let Some(item) = sent {
            sqlx::query("UPDATE outbox SET state='sent',body='' WHERE id=?")
                .bind(item.id)
                .execute(&db)
                .await
                .unwrap();
        } else {
            let state: String =
                sqlx::query_scalar("SELECT notification_state FROM schedule_runs WHERE task=?")
                    .bind(task.id)
                    .fetch_one(&db)
                    .await
                    .unwrap();
            assert_eq!(state, "unchanged");
        }
    }
}
