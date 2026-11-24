use super::{app_with_model, fixture, message, pending_id};
use crate::{model::ModelProvider, runtime, schedules, security::now, store};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

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
