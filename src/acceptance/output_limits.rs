use super::{app_with_model, fixture, message};
use crate::{
    audit,
    model::{ModelProvider, Models, ProviderFailure, TextDelta, ToolDefinition},
    runtime, store,
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const GOAL: &str = "Compare three train routes";
const SAVED: &str = "I saved the goal to compare three train routes.";
const OLD_CONTEXT: &str = "Long context fixture. ";

enum Stage {
    Limit,
    LimitWithLongContext,
    CompactReplanTimeout,
    CompactFinishTimeout,
    Incomplete,
    LookupLimit,
    ShortAnswer {
        text: &'static str,
        without_tools: bool,
    },
    SaveGoal {
        during_replan: bool,
    },
    ConfirmGoal {
        without_tools: bool,
    },
    RepeatGoalWithoutTools,
}

struct LimitModel {
    steps: Vec<Stage>,
    calls: AtomicUsize,
}

#[async_trait]
impl ModelProvider for LimitModel {
    async fn complete(&self, instructions: &str, _: &str) -> Result<String> {
        let index = self.calls.fetch_add(1, Ordering::Relaxed);
        assert!(matches!(self.steps[index], Stage::LookupLimit));
        assert!(instructions.contains("Use only evidence"));
        Err(ProviderFailure::OutputLimit.into())
    }

    async fn complete_turn(
        &self,
        instructions: &str,
        input: &str,
        tools: &[ToolDefinition],
        _: Option<&mut TextDelta<'_>>,
    ) -> Result<String> {
        let index = self.calls.fetch_add(1, Ordering::Relaxed);
        let plan = instructions.contains("take one small next step");
        let reply = match &self.steps[index] {
            Stage::Limit => return Err(ProviderFailure::OutputLimit.into()),
            Stage::LimitWithLongContext => {
                assert!(input.contains(&OLD_CONTEXT.repeat(500)));
                assert!(serde_json::from_str::<Value>(input)?["tools"].is_array());
                return Err(ProviderFailure::OutputLimit.into());
            }
            Stage::CompactReplanTimeout => {
                assert!(plan && !tools.is_empty());
                assert!(!input.contains(&OLD_CONTEXT.repeat(100)));
                let context: Value = serde_json::from_str(input)?;
                assert_eq!(context["latest_request"], "Wie is Yeat?");
                assert!(!context.as_object().unwrap().contains_key("tools"));
                return Err(ProviderFailure::TimedOut.into());
            }
            Stage::CompactFinishTimeout => {
                assert!(tools.is_empty());
                assert!(!input.contains(&OLD_CONTEXT.repeat(100)));
                return Err(ProviderFailure::TimedOut.into());
            }
            Stage::Incomplete => return Err(ProviderFailure::Incomplete.into()),
            Stage::LookupLimit => panic!("expected the saved-work finalizer"),
            Stage::ShortAnswer {
                text,
                without_tools,
            } => {
                assert_eq!(plan, !without_tools);
                assert_eq!(tools.is_empty(), *without_tools);
                assert!(input.contains("Wie is Yeat?"));
                assert!(!input.contains(&OLD_CONTEXT.repeat(100)));
                json!({"kind":"reply","text":text})
            }
            Stage::SaveGoal { during_replan } => {
                assert_eq!(plan, *during_replan);
                assert!(tools.iter().any(|tool| tool.name == "set_goal"));
                json!({"kind":"tool","name":"set_goal","args":{"text":GOAL}})
            }
            Stage::ConfirmGoal { without_tools } => {
                assert_eq!(tools.is_empty(), *without_tools);
                assert_eq!(plan, !without_tools);
                let context: Value = serde_json::from_str(input)?;
                assert_eq!(context["goal"], GOAL);
                assert!(context["history"].as_array().unwrap().iter().any(|entry| {
                    entry["role"] == "tool"
                        && entry["text"].as_str().unwrap().contains(r#""saved":true"#)
                }));
                json!({"kind":"reply","text":SAVED})
            }
            Stage::RepeatGoalWithoutTools => {
                assert!(tools.is_empty());
                json!({"kind":"tool","name":"set_goal","args":{"text":"Should stay unchanged"}})
            }
        };
        Ok(reply.to_string())
    }
}

#[tokio::test]
async fn a_quick_question_gets_a_concise_automatic_answer() {
    let reply = "Yeat is een Amerikaanse rapper.";
    for without_tools in [false, true] {
        let db = fixture().await;
        let mut steps = vec![Stage::Limit];
        if without_tools {
            steps.push(Stage::Limit);
        }
        steps.push(Stage::ShortAnswer {
            text: reply,
            without_tools,
        });
        let model = Arc::new(LimitModel {
            steps,
            calls: AtomicUsize::new(0),
        });
        let app = app_with_model(&db, model.clone());
        message(&db, 1, "1", "Wie is Yeat?").await;
        let task = store::claim(&db).await.unwrap().unwrap();
        runtime::work(&app, &task).await.unwrap();
        assert_eq!(store::claim_outbox(&db).await.unwrap().unwrap().body, reply);
        assert_eq!(model.calls.load(Ordering::Relaxed), model.steps.len());
    }
}

#[tokio::test]
async fn a_timed_out_smaller_plan_gets_a_tool_free_reply() {
    let db = fixture().await;
    let reply = "Yeat is een Amerikaanse rapper.";
    let model = Arc::new(LimitModel {
        steps: vec![
            Stage::LimitWithLongContext,
            Stage::CompactReplanTimeout,
            Stage::ShortAnswer {
                text: reply,
                without_tools: true,
            },
        ],
        calls: AtomicUsize::new(0),
    });
    let app = app_with_model(&db, model.clone());
    message(&db, 1, "1", "Wie is Yeat?").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    store::append(&db, &task.id, "assistant", &OLD_CONTEXT.repeat(500))
        .await
        .unwrap();

    runtime::work(&app, &task).await.unwrap();

    assert_eq!(store::claim_outbox(&db).await.unwrap().unwrap().body, reply);
    assert_eq!(model.calls.load(Ordering::Relaxed), 3);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM task_tool_attempts WHERE task=?")
        .bind(&task.id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let recovery: Vec<String> = sqlx::query_scalar(
        "SELECT status FROM audit_entries WHERE name='output_limit_recovery' AND kind='model'
         AND status IN ('completed','failed') ORDER BY id",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(recovery, ["failed", "completed"]);
}

#[tokio::test]
async fn a_configured_backup_can_finish_a_timed_out_recovery() {
    let db = fixture().await;
    let reply = "Yeat is een Amerikaanse rapper.";
    let primary = Arc::new(LimitModel {
        steps: vec![
            Stage::LimitWithLongContext,
            Stage::CompactReplanTimeout,
            Stage::CompactFinishTimeout,
        ],
        calls: AtomicUsize::new(0),
    });
    let backup = Arc::new(LimitModel {
        steps: vec![Stage::ShortAnswer {
            text: reply,
            without_tools: false,
        }],
        calls: AtomicUsize::new(0),
    });
    let mut app = app_with_model(&db, primary.clone());
    app.models = Models::new(db.clone(), app.vault.clone(), "codex".into(), None, None)
        .with_fixture(primary.clone())
        .with_backup_fixture(backup.clone());
    sqlx::query(
        "UPDATE users SET codex_model='test-backup',codex_credential='fixture' WHERE id='1'",
    )
    .execute(&db)
    .await
    .unwrap();
    app.models
        .save_backup("1", "codex", "test-backup")
        .await
        .unwrap();
    message(&db, 1, "1", "Wie is Yeat?").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    store::append(&db, &task.id, "assistant", &OLD_CONTEXT.repeat(500))
        .await
        .unwrap();

    runtime::work(&app, &task).await.unwrap();

    let notice = store::claim_outbox(&db).await.unwrap().unwrap();
    assert!(notice.body.contains("ChatGPT for Codex · test-backup"));
    assert_eq!(store::claim_outbox(&db).await.unwrap().unwrap().body, reply);
    assert_eq!(primary.calls.load(Ordering::Relaxed), 3);
    assert_eq!(backup.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn a_smaller_plan_uses_the_normal_tool_broker() {
    let db = fixture().await;
    let model = Arc::new(LimitModel {
        steps: vec![
            Stage::Limit,
            Stage::SaveGoal {
                during_replan: true,
            },
            Stage::ConfirmGoal {
                without_tools: false,
            },
        ],
        calls: AtomicUsize::new(0),
    });
    let app = app_with_model(&db, model.clone());
    message(&db, 1, "1", "Save the goal to compare three train routes.").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    assert_eq!(store::claim_outbox(&db).await.unwrap().unwrap().body, SAVED);
    assert_eq!(model.calls.load(Ordering::Relaxed), 3);
    let tool: String = sqlx::query_scalar("SELECT tool FROM task_tool_attempts WHERE task=?")
        .bind(&task.id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(tool, "set_goal");
}

#[tokio::test]
async fn saved_work_is_reported_without_enabling_more_tools() {
    for (completion, succeeds) in [
        (
            Stage::ConfirmGoal {
                without_tools: true,
            },
            true,
        ),
        (Stage::RepeatGoalWithoutTools, false),
    ] {
        let db = fixture().await;
        let model = Arc::new(LimitModel {
            steps: vec![
                Stage::SaveGoal {
                    during_replan: false,
                },
                Stage::Limit,
                completion,
            ],
            calls: AtomicUsize::new(0),
        });
        let app = app_with_model(&db, model.clone());
        message(&db, 1, "1", "Save the goal to compare three train routes.").await;
        let task = store::claim(&db).await.unwrap().unwrap();
        assert_eq!(runtime::work(&app, &task).await.is_ok(), succeeds);
        assert_eq!(model.calls.load(Ordering::Relaxed), 3);
        let (goal, state): (String, String) =
            sqlx::query_as("SELECT goal,state FROM tasks WHERE id=?")
                .bind(&task.id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(goal, GOAL);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM task_tool_attempts WHERE task=?")
            .bind(&task.id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 1);
        let session = audit::sessions(&db, "1", None).await.unwrap()["sessions"][0]["id"]
            .as_i64()
            .unwrap();
        let audit = audit::session(&db, "1", session, 0).await.unwrap();
        let item = store::claim_outbox(&db).await.unwrap().unwrap();
        if succeeds {
            assert_eq!(state, "idle");
            assert_eq!(item.body, SAVED);
        } else {
            assert_eq!(state, "blocked");
            assert!(audit["entries"].as_array().unwrap().iter().any(|entry| {
                entry["name"] == "output_limit_recovery" && entry["status"] == "failed"
            }));
            let token = item
                .button
                .as_deref()
                .unwrap()
                .strip_prefix(store::task_failures::RETRY)
                .unwrap();
            let (notice, _) = store::task_failures::notice(&db, &item, token)
                .await
                .unwrap()
                .unwrap();
            assert!(notice.contains("response limit"));
            assert!(notice.contains("shorter reply"));
            assert!(notice.contains("already saved the task goal"));
            assert!(!notice.contains("split the request"));
        }
    }
}

#[tokio::test]
async fn a_limit_in_the_saved_work_finalizer_gets_a_compact_retry() {
    let db = fixture().await;
    let model = Arc::new(LimitModel {
        steps: vec![
            Stage::SaveGoal {
                during_replan: false,
            },
            Stage::Incomplete,
            Stage::LookupLimit,
            Stage::ConfirmGoal {
                without_tools: true,
            },
        ],
        calls: AtomicUsize::new(0),
    });
    let app = app_with_model(&db, model.clone());
    message(&db, 1, "1", "Save the goal to compare three train routes.").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    assert_eq!(store::claim_outbox(&db).await.unwrap().unwrap().body, SAVED);
    assert_eq!(model.calls.load(Ordering::Relaxed), 4);
}
