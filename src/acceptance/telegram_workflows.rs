use super::*;
use crate::store::{groups::GroupRoute, outputs, task_views};

struct ReplyChannel(tokio::sync::mpsc::UnboundedSender<String>);

#[async_trait]
impl crate::channel::ChannelAdapter for ReplyChannel {
    fn name(&self) -> &'static str {
        "telegram"
    }

    async fn receive(&self, _: i64) -> anyhow::Result<(Vec<crate::channel::Inbound>, i64)> {
        unreachable!()
    }

    async fn send(
        &self,
        _: &str,
        body: &str,
        _: Option<&str>,
        _: Option<i64>,
    ) -> anyhow::Result<crate::channel::MessageDelivery> {
        self.0.send(body.to_owned())?;
        Ok(crate::channel::MessageDelivery::Sent(None))
    }
}

#[tokio::test]
async fn split_delivery_retries_recording_an_outcome_without_resending() {
    let db = fixture().await;
    let app = Arc::new(app_with_model(&db, Arc::new(GroupChief)));
    message(&db, 1, "1", "Write a long summary.").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    let first = "a".repeat(4095);
    store::settle_reply(&db, &task, &format!("{first}\nSecond part"), &[])
        .await
        .unwrap();
    sqlx::raw_sql(
        "CREATE TABLE delivery_fixture_failures (id INTEGER PRIMARY KEY);
         CREATE TRIGGER fail_delivery_write_once BEFORE UPDATE OF state ON outbox
         WHEN NEW.state='sent' AND NOT EXISTS (SELECT 1 FROM delivery_fixture_failures)
         BEGIN
           INSERT INTO delivery_fixture_failures VALUES (OLD.id);
           SELECT RAISE(FAIL, 'fixture delivery write failure');
         END;",
    )
    .execute(&db)
    .await
    .unwrap();

    let (send, mut received) = tokio::sync::mpsc::unbounded_channel();
    let worker = tokio::spawn(crate::channel::outbox(app, Arc::new(ReplyChannel(send))));
    let parts = tokio::time::timeout(Duration::from_secs(3), async {
        [
            received.recv().await.unwrap(),
            received.recv().await.unwrap(),
        ]
    })
    .await;
    worker.abort();

    let [first_part, last_part] = parts.unwrap();
    assert_eq!(first_part.trim_end(), first);
    assert_eq!(last_part, "Second part");
    let saved_first: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM outbox o JOIN delivery_fixture_failures f ON f.id=o.id
                       WHERE o.state='sent')",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert!(saved_first);
}

struct GroupChief;

#[async_trait]
impl ModelProvider for GroupChief {
    async fn complete(&self, _: &str, _: &str) -> Result<String> {
        unreachable!()
    }

    async fn complete_turn(
        &self,
        instructions: &str,
        input: &str,
        tools: &[ToolDefinition],
        _delta: Option<&mut TextDelta<'_>>,
    ) -> Result<String> {
        assert!(instructions.contains("shared Telegram group"));
        assert!(input.contains("Draft the group summary"));
        assert!(!input.contains("PRIVATE-PERSONAL"));
        assert_eq!(
            tools.iter().map(|tool| tool.name).collect::<Vec<_>>(),
            ["browse", "search_web"]
        );
        Ok(json!({"kind":"reply","text":"Group draft is ready."}).to_string())
    }
}

#[tokio::test]
async fn group_tasks_require_registration_and_isolate_topics_accounts_and_private_history() {
    let db = fixture().await;
    let app = app_with_model(&db, Arc::new(GroupChief));
    let route = GroupRoute {
        chat: -1020,
        topic: 12,
    };
    assert!(
        !store::groups::input(&db, "2", 1, route, 42, None, "Draft the group summary")
            .await
            .unwrap()
    );
    assert!(
        store::groups::request(&db, "1", "2", route.chat)
            .await
            .unwrap()
    );
    message(&db, 2, "2", "/approve_group -1020").await;
    assert!(
        !store::groups::input(&db, "2", 3, route, 43, None, "Draft the group summary")
            .await
            .unwrap()
    );
    message(&db, 4, "1", "/approve_group -1020").await;
    message(&db, 5, "2", "PRIVATE-PERSONAL context").await;
    let private = store::claim(&db).await.unwrap().unwrap();
    store::settle(&db, &private, "idle", Some("PRIVATE-PERSONAL answer"))
        .await
        .unwrap();

    assert!(
        store::groups::input(&db, "2", 6, route, 44, None, "Draft the group summary")
            .await
            .unwrap()
    );
    let task = store::claim(&db).await.unwrap().unwrap();
    assert!(!store::select_task(&db, "2", &task.id).await.unwrap());
    assert!(
        task_views::search(&db, "2", "group summary", 0)
            .await
            .unwrap()
            .is_empty()
    );
    runtime::work(&app, &task).await.unwrap();
    let item: store::Outbound = sqlx::query_as(
        "SELECT id,user,agent,task,body,button,widget,reaction_event,reply_to_message_id FROM outbox WHERE task=?",
    ).bind(&task.id).fetch_one(&db).await.unwrap();
    assert_eq!(item.body, "Group draft is ready.");
    assert_eq!(item.reply_to_message_id, Some(44));
    let destination = store::groups::destination(&db, &item)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((destination.chat, destination.topic), (-1020, 12));
    sqlx::query("UPDATE outbox SET state='sent',telegram_group_message_id=45 WHERE id=?")
        .bind(item.id)
        .execute(&db)
        .await
        .unwrap();
    assert!(
        !store::groups::input(
            &db,
            "2",
            7,
            GroupRoute {
                chat: route.chat,
                topic: 13
            },
            46,
            Some(45),
            "Wrong topic"
        )
        .await
        .unwrap()
    );
    assert!(
        !store::groups::input(&db, "1", 8, route, 47, Some(45), "Wrong requester")
            .await
            .unwrap()
    );
    assert!(
        store::groups::input(&db, "2", 9, route, 48, Some(45), "Follow up")
            .await
            .unwrap()
    );
    message(&db, 10, "1", "/disable_group -1020").await;
    assert!(store::groups::destination(&db, &item).await.is_err());
    assert!(
        !store::groups::input(&db, "2", 11, route, 49, None, "Disabled")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn task_status_uses_reply_target_and_stale_stop_cannot_cancel_a_new_turn() {
    let db = fixture().await;
    message(&db, 1, "2", "First task").await;
    let first = store::claim(&db).await.unwrap().unwrap();
    store::settle(&db, &first, "idle", Some("First reply"))
        .await
        .unwrap();
    sqlx::query("UPDATE outbox SET state='sent',telegram_message_id=13 WHERE task=?")
        .bind(&first.id)
        .execute(&db)
        .await
        .unwrap();
    message(&db, 2, "2", "/new Second task").await;
    let second = store::claim(&db).await.unwrap().unwrap();
    store::ingest_telegram_reply(
        &db,
        "1",
        3,
        "2",
        "/status",
        21,
        Some(store::ReplyContext {
            message_id: 13,
            excerpt: None,
            from_bot: true,
        }),
    )
    .await
    .unwrap();
    let notice = store::claim_outbox(&db).await.unwrap().unwrap();
    assert_eq!(notice.task.as_deref(), Some(first.id.as_str()));
    assert!(notice.body.contains("Ready for a follow-up"));
    assert!(
        !task_views::stop(&db, "1", &second.id, second.revision)
            .await
            .unwrap()
    );
    message(&db, 4, "2", "Steer the second task").await;
    assert!(
        !task_views::stop(&db, "2", &second.id, second.revision)
            .await
            .unwrap()
    );
    assert_eq!(
        task_views::search(&db, "2", "steer the second task", 0)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        task_views::search(&db, "1", "second task", 0)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        task_views::stop(&db, "2", &second.id, second.revision + 1)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn finishing_a_quoted_task_keeps_the_next_message_with_its_selected_task() {
    let db = fixture().await;
    store::ingest_telegram(&db, "1", 1, "2", "Write the first summary", 11)
        .await
        .unwrap();
    let first = store::claim(&db).await.unwrap().unwrap();
    store::settle_reply(&db, &first, "First summary", &[])
        .await
        .unwrap();
    sqlx::query(
        "UPDATE outbox SET state='sent',telegram_message_id=12 WHERE task=? AND assistant_event_id IS NOT NULL",
    )
    .bind(&first.id)
    .execute(&db)
    .await
    .unwrap();
    message(&db, 2, "2", "/new A second task").await;
    let second = store::claim(&db).await.unwrap().unwrap();

    store::ingest_telegram_reply(
        &db,
        "1",
        3,
        "2",
        "/done",
        13,
        Some(store::ReplyContext {
            message_id: 12,
            excerpt: None,
            from_bot: true,
        }),
    )
    .await
    .unwrap();
    let completed = task_for(&db, &first.id).await;
    assert_eq!(completed.state, "completed");
    assert_eq!(completed.revision, first.revision + 1);
    assert_eq!(task_for(&db, &second.id).await.state, "running");
    assert_eq!(
        task_views::search_in_scope(&db, "2", "first summary", 0, true)
            .await
            .unwrap()
            .len(),
        1
    );
    store::ingest_telegram_reaction(&db, "1", 4, "2", 12, "👍")
        .await
        .unwrap();
    assert_eq!(task_for(&db, &first.id).await.state, "completed");
    message(&db, 5, "2", "Another detail for the second task").await;
    assert_eq!(
        store::history(&db, &second.id)
            .await
            .unwrap()
            .last()
            .unwrap()
            .2,
        "Another detail for the second task"
    );
    message(&db, 6, "2", "/done").await;
    assert_eq!(task_for(&db, &second.id).await.state, "completed");
    message(&db, 7, "2", "An unrelated task").await;
    let unrelated = store::claim(&db).await.unwrap().unwrap();
    assert_ne!(unrelated.id, first.id);
    assert_ne!(unrelated.id, second.id);

    assert!(store::select_task(&db, "2", &first.id).await.unwrap());
    assert_eq!(task_for(&db, &first.id).await.state, "idle");
    message(&db, 8, "2", "A follow-up for the first summary").await;
    let reopened = task_for(&db, &first.id).await;
    assert_eq!(reopened.state, "queued");
    assert_eq!(store::history(&db, &first.id).await.unwrap().len(), 3);
}

#[tokio::test]
async fn task_completion_checks_the_owner_revision_and_pending_actions() {
    let db = fixture().await;
    message(&db, 1, "1", "Review the report").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    assert!(
        !task_views::complete(&db, "2", &task.id, task.revision)
            .await
            .unwrap()
    );
    assert!(
        !task_views::complete(&db, "1", &task.id, task.revision - 1)
            .await
            .unwrap()
    );
    assert!(store::start_action(&db, &task, "in-flight").await.unwrap());
    assert!(
        !task_views::complete(&db, "1", &task.id, task.revision)
            .await
            .unwrap()
    );
    store::finish_action(&db, &task, "in-flight", "Action finished.", false)
        .await
        .unwrap();
    store::pause(
        &db,
        &task,
        "approve",
        &json!({}),
        "Review this action.",
        None,
        600,
        "approval",
    )
    .await
    .unwrap();
    assert!(
        task_views::complete(&db, "1", &task.id, task.revision)
            .await
            .unwrap()
    );
    assert_eq!(task_for(&db, &task.id).await.state, "completed");
    let state: String = sqlx::query_scalar("SELECT state FROM pending WHERE id='approval'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(state, "superseded");
    let entry: String = sqlx::query_scalar(
        "SELECT status FROM audit_entries WHERE kind='task' AND name='marked_complete'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(entry, "completed");
}

#[tokio::test]
async fn signed_task_completion_moves_only_the_owners_task_into_history() {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower_service::Service;

    let db = fixture().await;
    let app = Arc::new(app_with_model(&db, Arc::new(GroupChief)));
    message(&db, 1, "1", "Review a signed report").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    store::settle(&db, &task, "idle", None).await.unwrap();
    let mut router = web::router(app.clone());
    let request = |user, action, data| {
        Request::post("/api/mini")
            .header("origin", &app.origin)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"init":signed_mini_init(&app,user),"action":action,"data":data}).to_string(),
            ))
            .unwrap()
    };
    let target = json!({"task":task.id,"revision":task.revision});
    assert!(
        router
            .call(request(2, "complete-task", target.clone()))
            .await
            .unwrap()
            .status()
            .is_client_error()
    );
    assert_eq!(task_for(&db, &task.id).await.state, "idle");
    assert!(
        router
            .call(request(1, "complete-task", target))
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let response = router
        .call(request(1, "status", Value::Null))
        .await
        .unwrap();
    let state: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert!(state["active_task"].is_null());
    let response = router
        .call(request(
            1,
            "search-tasks",
            json!({"query":"","completed_only":true}),
        ))
        .await
        .unwrap();
    let archive: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(archive["items"][0]["id"], task.id);
    assert_eq!(archive["items"][0]["state"], "completed");
}

#[tokio::test]
async fn task_output_is_encrypted_and_immutable_and_recovers_uncertain_delivery() {
    let db = fixture().await;
    let app = app_with_model(&db, Arc::new(FileChief));
    let vault = &app.vault;
    message(&db, 1, "1", "Send my CSV as a file").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    sqlx::query("INSERT INTO files VALUES(?,'report.csv','col1,col2')")
        .bind(&task.id)
        .execute(&db)
        .await
        .unwrap();
    runtime::work(&app, &task).await.unwrap();
    sqlx::query("UPDATE files SET body='new version' WHERE task=?")
        .bind(&task.id)
        .execute(&db)
        .await
        .unwrap();
    let item = store::claim_outbox(&db).await.unwrap().unwrap();
    let snapshot = outputs::load(&db, vault, &item).await.unwrap().unwrap();
    assert_eq!(snapshot.bytes, b"col1,col2");
    let ciphertext: String =
        sqlx::query_scalar("SELECT ciphertext FROM telegram_outputs WHERE outbox=?")
            .bind(item.id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(!ciphertext.contains("col1,col2"));
    store::recover(&db).await.unwrap();
    assert!(outputs::load(&db, vault, &item).await.unwrap().is_none());
    let notice = store::claim_outbox(&db).await.unwrap().unwrap();
    assert!(notice.body.contains("may have received"));
    assert!(store::claim_outbox(&db).await.unwrap().is_none());
}

struct FileChief;

#[async_trait]
impl ModelProvider for FileChief {
    async fn complete(&self, _: &str, input: &str) -> Result<String> {
        let data: Value = serde_json::from_str(input)?;
        let final_event = data["history"].as_array().unwrap().last().unwrap();
        Ok(if final_event["role"] == "user" {
            json!({"kind":"tool","name":"send_task_file","args":{"name":"report.csv"}})
        } else {
            let result: Value = serde_json::from_str(final_event["text"].as_str().unwrap())?;
            assert_eq!(result["queued"], true);
            json!({"kind":"reply","text":""})
        }
        .to_string())
    }
}

struct UnavailableModel;

#[async_trait]
impl ModelProvider for UnavailableModel {
    async fn complete(&self, _: &str, _: &str) -> Result<String> {
        Err(crate::model::ProviderFailure::Temporary.into())
    }
}

#[tokio::test]
async fn a_model_failure_retries_the_same_saved_request_with_a_fresh_button() {
    use crate::store::task_failures;
    let db = fixture().await;
    let app = app_with_model(&db, Arc::new(UnavailableModel));
    message(&db, 1, "2", "Create a summary for tomorrow").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    assert!(runtime::work(&app, &task).await.is_err());
    assert_eq!(task_for(&db, &task.id).await.state, "blocked");
    let token: String =
        sqlx::query_scalar("SELECT token FROM task_failures WHERE task=? AND retryable=1")
            .bind(&task.id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(!task_failures::retry(&db, "1", &token).await.unwrap());
    assert!(task_failures::retry(&db, "2", &token).await.unwrap());
    assert!(!task_failures::retry(&db, "2", &token).await.unwrap());
    let retry = store::claim(&db).await.unwrap().unwrap();
    assert_eq!((&retry.id, retry.revision), (&task.id, task.revision));
    assert_eq!(store::history(&db, &task.id).await.unwrap().len(), 1);
    assert!(runtime::work(&app, &retry).await.is_err());
    assert!(!task_failures::retry(&db, "2", &token).await.unwrap());
    let current: String = sqlx::query_scalar("SELECT token FROM task_failures WHERE task=?")
        .bind(&task.id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_ne!(current, token);
    message(&db, 2, "2", "Actually, summarize today").await;
    assert!(!task_failures::retry(&db, "2", &current).await.unwrap());
}

#[tokio::test]
async fn a_runtime_failure_preserves_uncertainty_for_an_approved_action() {
    let db = fixture().await;
    message(&db, 1, "1", "Run the action").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    store::pause(
        &db,
        &task,
        "approve",
        &json!({}),
        "Review",
        None,
        900,
        "execution",
    )
    .await
    .unwrap();
    store::respond(&db, "1", "execution", true).await.unwrap();
    let task = store::claim(&db).await.unwrap().unwrap();
    assert!(store::claim_approval(&db, &task).await.unwrap().is_some());
    store::task_failures::record(&db, &task, "The model is temporarily unavailable.", true)
        .await
        .unwrap();
    assert_eq!(task_for(&db, &task.id).await.state, "uncertain");
    assert!(
        sqlx::query_scalar::<_, String>("SELECT token FROM task_failures WHERE task=?")
            .bind(&task.id)
            .fetch_optional(&db)
            .await
            .unwrap()
            .is_none()
    );
    message(&db, 2, "1", "/retry").await;
    assert_eq!(task_for(&db, &task.id).await.state, "uncertain");
}
