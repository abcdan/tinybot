use super::*;
use crate::runtime::{Intent, Outcome, Risk, Tool, ToolContext};
use std::{
    future::pending,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Planner(AtomicUsize);

#[async_trait]
impl ModelProvider for Planner {
    async fn complete(&self, _: &str, body: &str) -> Result<String> {
        let input: Value = serde_json::from_str(body)?;
        let step = match self.0.fetch_add(1, Ordering::Relaxed) {
            0 => {
                json!({"kind":"tool","name":"plan_task","args":{"steps":["Read report","Draft summary"]}})
            }
            1 => {
                assert_eq!(input["task_plan"][0]["title"], "Read report");
                json!({"kind":"tool","name":"record_task_checkpoint","args":{"position":0,"checkpoint":"The report lists two decisions."}})
            }
            2 => {
                assert_eq!(
                    input["task_plan"][0]["checkpoint"],
                    "The report lists two decisions."
                );
                assert!(input["task_plan"][1]["completed_at"].is_null());
                json!({"kind":"reply","text":"The report has two decisions. The summary is still open."})
            }
            _ => panic!("unexpected model turn"),
        };
        Ok(step.to_string())
    }
}

#[tokio::test]
async fn recorded_work_is_visible_in_telegram_and_completed_history() {
    let db = fixture().await;
    let app = app_with_model(&db, Arc::new(Planner(AtomicUsize::new(0))));
    message(&db, 1, "1", "Read the report and then draft a summary.").await;
    let task = store::claim(&db).await.unwrap().unwrap();

    runtime::work(&app, &task).await.unwrap();
    assert!(
        store::claim_outbox(&db)
            .await
            .unwrap()
            .unwrap()
            .body
            .contains("summary is still open")
    );
    message(&db, 2, "1", "/status").await;
    let status = store::claim_outbox(&db).await.unwrap().unwrap().body;
    assert!(status.contains("✓ Read report"));
    assert!(status.contains("○ Draft summary"));
    assert!(
        store::task_views::complete(&db, "1", &task.id, task.revision)
            .await
            .unwrap()
    );
    let detail = store::task_views::detail(&db, "1", &task.id)
        .await
        .unwrap()
        .unwrap();
    let detail = serde_json::to_value(detail).unwrap();
    assert_eq!(
        detail["plan"][0]["checkpoint"],
        "The report lists two decisions."
    );
    assert!(
        store::task_views::detail(&db, "2", &task.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(store::select_task(&db, "1", &task.id).await.unwrap());
    let reopened = task_for(&db, &task.id).await;
    assert!(
        store::checkpoints::plan(&db, &task.id, reopened.revision, false)
            .await
            .unwrap()
            .is_empty()
    );
}

struct ReadPage(Arc<AtomicUsize>);

#[async_trait]
impl Tool for ReadPage {
    fn name(&self) -> &'static str {
        "browse"
    }
    fn describe(&self) -> &'static str {
        "Read a public page."
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object"})
    }

    async fn inspect(&self, _: &ToolContext<'_>, _: &Value) -> Result<Intent> {
        Ok(Intent {
            risk: Risk::PublicRead,
            destination: "Fixture page".into(),
            detail: "read".into(),
        })
    }

    async fn execute(&self, _: &ToolContext<'_>, _: Value) -> Result<Outcome> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(Outcome::Continue(
            json!({"url":"https://example.org/report","title":"Report","text":"The report lists two decisions."}),
        ))
    }
}

struct RestartingChief {
    steps: AtomicUsize,
    read_saved: Notify,
}

#[async_trait]
impl ModelProvider for RestartingChief {
    async fn complete(&self, _: &str, body: &str) -> Result<String> {
        let input: Value = serde_json::from_str(body)?;
        let lookup = || {
            json!({"kind":"tool","name":"browse","args":{"url":"https://example.org/report"}})
                .to_string()
        };
        match self.steps.fetch_add(1, Ordering::Relaxed) {
            0 => Ok(lookup()),
            1 => {
                self.read_saved.notify_one();
                pending().await
            }
            2 => {
                assert_eq!(
                    input["verified_reads"][0]["result"]["text"],
                    "The report lists two decisions."
                );
                Ok(lookup())
            }
            3 => {
                assert_eq!(input["verified_reads"][0]["result"]["title"], "Report");
                Ok(json!({"kind":"reply","text":"The report lists two decisions."}).to_string())
            }
            _ => panic!("unexpected model turn"),
        }
    }
}

#[tokio::test]
async fn a_confirmed_public_read_is_reused_after_restart_only_for_the_same_revision() {
    let db = fixture().await;
    let chief = Arc::new(RestartingChief {
        steps: AtomicUsize::new(0),
        read_saved: Notify::new(),
    });
    let reads = Arc::new(AtomicUsize::new(0));
    let mut app = app_with_model(&db, chief.clone());
    app.tools.retain(|tool| tool.name() != "browse");
    app.tools.push(Box::new(ReadPage(reads.clone())));
    let app = Arc::new(app);
    message(&db, 1, "1", "Open the public report and summarize it.").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    let work = tokio::spawn({
        let app = app.clone();
        let task = task.clone();
        async move { runtime::work(&app, &task).await }
    });
    tokio::time::timeout(Duration::from_secs(3), chief.read_saved.notified())
        .await
        .unwrap();
    work.abort();
    assert!(work.await.unwrap_err().is_cancelled());
    assert_eq!(reads.load(Ordering::Relaxed), 1);
    let encrypted: String =
        sqlx::query_scalar("SELECT ciphertext FROM task_verified_reads WHERE task=?")
            .bind(&task.id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(!encrypted.contains("two decisions"));

    store::recover(&db).await.unwrap();
    let resumed = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(resumed.revision, task.revision);
    runtime::work(&app, &resumed).await.unwrap();

    assert_eq!(reads.load(Ordering::Relaxed), 1);
    assert_eq!(
        store::claim_outbox(&db).await.unwrap().unwrap().body,
        "The report lists two decisions."
    );
    let passes: i64 = sqlx::query_scalar("SELECT passes FROM task_model_passes WHERE task=?")
        .bind(&task.id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(passes, 4);
    message(&db, 2, "1", "Read it again for changes.").await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM task_verified_reads WHERE task=?")
        .bind(&task.id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn a_read_finishing_after_steering_does_not_attach_to_the_new_revision() {
    let db = fixture().await;
    let vault = security::Vault::new(&"f0".repeat(32)).unwrap();
    message(&db, 1, "1", "Read the original report.").await;
    let original = store::claim(&db).await.unwrap().unwrap();
    assert!(
        store::start_action(&db, &original, "original-read")
            .await
            .unwrap()
    );
    message(&db, 2, "1", "Switch to the new report.").await;

    store::checkpoints::finish_read(
        &db,
        &vault,
        &original,
        "original-read",
        "The first report mentioned confidential details.",
        Some((
            "old-key",
            r#"{"tool":"browse","result":{"text":"First report"}}"#,
        )),
    )
    .await
    .unwrap();
    store::settle(&db, &original, "idle", None).await.unwrap();
    let current = store::claim(&db).await.unwrap().unwrap();

    assert!(current.revision > original.revision);
    assert!(
        store::checkpoints::read_cache(&db, &vault, &current)
            .await
            .unwrap()
            .keys
            .is_empty()
    );
    let history = store::history(&db, &current.id).await.unwrap();
    assert_eq!(history.last().unwrap().2, "Switch to the new report.");
}

struct FinalizeAtLimit;

#[async_trait]
impl ModelProvider for FinalizeAtLimit {
    async fn complete(&self, system: &str, body: &str) -> Result<String> {
        assert!(system.contains("Give a final answer"));
        let input: Value = serde_json::from_str(body)?;
        assert_eq!(
            input["tool_stop_reason"],
            "This turn reached its model pass budget."
        );
        assert_eq!(input["task_plan"][0]["title"], "Read report");
        Err(crate::model::ProviderFailure::Temporary.into())
    }
}

#[tokio::test]
async fn a_turn_at_its_model_budget_keeps_completed_work_even_if_the_finalizer_fails() {
    let db = fixture().await;
    let app = app_with_model(&db, Arc::new(FinalizeAtLimit));
    message(&db, 1, "1", "Read the report, then draft a summary.").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    store::checkpoints::save_plan(&db, &task, &["Read report", "Draft summary"])
        .await
        .unwrap();
    assert!(
        store::checkpoints::record_step(&db, &task, 1, "Unexpected")
            .await
            .is_err()
    );
    store::checkpoints::record_step(&db, &task, 0, "Two decisions recorded.")
        .await
        .unwrap();
    sqlx::query("INSERT INTO task_model_passes(task,revision,passes) VALUES(?,?,?)")
        .bind(&task.id)
        .bind(task.revision)
        .bind(store::checkpoints::MAX_MODEL_PASSES)
        .execute(&db)
        .await
        .unwrap();

    runtime::work(&app, &task).await.unwrap();

    let reply = store::claim_outbox(&db).await.unwrap().unwrap();
    assert!(reply.body.contains("Two decisions recorded."));
    assert!(reply.body.contains("Still open\n- Draft summary"));
    assert_eq!(task_for(&db, &task.id).await.state, "blocked");
}
