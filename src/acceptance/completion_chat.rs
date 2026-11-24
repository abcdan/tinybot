use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

const CONFIRMATION: &str = "Is gedaan, Chris heeft ’m ontvangen!";

struct CompletionModel {
    proposed: AtomicBool,
}

#[async_trait]
impl ModelProvider for CompletionModel {
    async fn complete(&self, _: &str, _: &str) -> Result<String> {
        unreachable!()
    }

    async fn complete_turn(
        &self,
        _: &str,
        _: &str,
        tools: &[ToolDefinition],
        _: Option<&mut TextDelta<'_>>,
    ) -> Result<String> {
        assert!(tools.iter().any(|tool| tool.name == "complete_task"));
        let reply = if self.proposed.swap(true, Ordering::Relaxed) {
            json!({"kind":"reply","text":"Ik zoek het op."})
        } else {
            json!({"kind":"tool","name":"complete_task","args":{
                "confirmation":CONFIRMATION,"reply":"Ik heb de taak afgevinkt."
            }})
        };
        Ok(reply.to_string())
    }
}

#[tokio::test]
async fn only_the_latest_telegram_confirmation_can_complete_a_task() {
    for (latest, final_state, reply) in [
        (CONFIRMATION, "completed", "Ik heb de taak afgevinkt."),
        ("Zoek ook de datum op.", "idle", "Ik zoek het op."),
    ] {
        let db = fixture().await;
        let app = app_with_model(
            &db,
            Arc::new(CompletionModel {
                proposed: AtomicBool::new(false),
            }),
        );
        let previous = if latest == CONFIRMATION {
            "Stuur Chris de link."
        } else {
            CONFIRMATION
        };
        store::ingest_telegram(&db, "1", 1, "1", previous, 19)
            .await
            .unwrap();
        let first = store::claim(&db).await.unwrap().unwrap();
        store::settle(&db, &first, "idle", None).await.unwrap();
        store::ingest_telegram(&db, "1", 2, "1", latest, 20)
            .await
            .unwrap();
        let task = store::claim(&db).await.unwrap().unwrap();
        assert_eq!(task.id, first.id);
        runtime::work(&app, &task).await.unwrap();
        assert_eq!(task_for(&db, &task.id).await.state, final_state);
        assert_eq!(store::claim_outbox(&db).await.unwrap().unwrap().body, reply);
        if final_state == "completed" {
            store::ingest_telegram(&db, "1", 3, "1", "Bekijk deze andere link.", 21)
                .await
                .unwrap();
            assert_ne!(store::claim(&db).await.unwrap().unwrap().id, task.id);
        }
    }
}
