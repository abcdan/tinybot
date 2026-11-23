use super::{app_with_model, fixture, message};
use crate::{model::ModelProvider, runtime, store};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct BusyChief(AtomicUsize);

#[async_trait]
impl ModelProvider for BusyChief {
    async fn complete(&self, system: &str, input: &str) -> Result<String> {
        if system.contains("Summarize only durable task context") {
            return Ok(json!({"summary":"The task is still collecting results."}).to_string());
        }
        let context: Value = serde_json::from_str(input)?;
        assert_eq!(
            context["latest_request"],
            "Summarize what you find about my sleep."
        );
        if system.contains("Give a final answer") {
            assert!(context["tool_stop_reason"].is_string());
            return Ok(
                json!({"text":"I couldn't find any sleep records to summarize."}).to_string(),
            );
        }
        let position = self.0.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(json!({"kind":"tool","name":"set_goal","args":{"text":format!("Inspect record {position}")}}).to_string())
    }
}

struct LoopDecisions {
    checks: AtomicUsize,
    stop_at: Option<usize>,
}

#[async_trait]
impl ModelProvider for LoopDecisions {
    async fn complete(&self, system: &str, input: &str) -> Result<String> {
        assert_eq!(system, "Jev tool loop decisions");
        let input: Value = serde_json::from_str(input)?;
        let attempts = input["state"]["attempts"].as_array().unwrap();
        assert_eq!(attempts.len(), 10);
        assert!(attempts.iter().all(|attempt| attempt["tool"] == "set_goal"));
        assert!(!input.to_string().contains("Inspect record"));
        let count = self.checks.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(json!({"stalled":{"type":"noul","noul":if self.stop_at == Some(count) {0.99} else {0.05}}}).to_string())
    }
}

#[tokio::test]
async fn chief_checks_every_ten_calls_and_finishes_from_context_at_a_loop_or_hard_cap() {
    for (stop_at, expected_calls) in [(Some(2), 20), (None, 100)] {
        let db = fixture().await;
        let chief = Arc::new(BusyChief(AtomicUsize::new(0)));
        let decisions = Arc::new(LoopDecisions {
            checks: AtomicUsize::new(0),
            stop_at,
        });
        let mut app = app_with_model(&db, chief.clone());
        app.models = app.models.with_memory_decisions(decisions.clone());
        message(&db, 1, "1", "Summarize what you find about my sleep.").await;
        let task = store::claim(&db).await.unwrap().unwrap();
        runtime::work(&app, &task).await.unwrap();
        let reply = store::claim_outbox(&db).await.unwrap().unwrap();
        assert_eq!(
            reply.body,
            "I couldn't find any sleep records to summarize."
        );
        assert_eq!(chief.0.load(Ordering::Relaxed), expected_calls);
        assert_eq!(
            decisions.checks.load(Ordering::Relaxed),
            expected_calls / 10
        );
        let retained: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM events WHERE task=? AND role='user' AND body LIKE '%my sleep.%')")
            .bind(&task.id).fetch_one(&db).await.unwrap();
        assert!(retained);
    }
}
