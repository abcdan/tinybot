use super::*;
use crate::model::{Models, ProviderFailure};
use std::sync::{Arc, Mutex};

const QUESTION: &str = "Wie is Yeat?";
const ANSWER: &str = "Yeat is een Amerikaanse rapper.";

struct QuickFixture {
    calls: Mutex<Vec<(bool, usize)>>,
    compact: &'static str,
}

#[async_trait]
impl ModelProvider for QuickFixture {
    async fn complete(&self, _: &str, _: &str) -> Result<String> {
        unreachable!("only the expected Chief turn may call the provider")
    }

    async fn complete_brief_reply(&self, instructions: &str, input: &str) -> Result<String> {
        assert!(instructions.contains("instead of an em dash in your own prose"));
        assert_eq!(
            serde_json::from_str::<Value>(input)?,
            json!({"request":QUESTION})
        );
        self.calls
            .lock()
            .unwrap()
            .push((true, instructions.len() + input.len()));
        if self.compact == "timeout" {
            return Err(ProviderFailure::TimedOut.into());
        }
        Ok(self.compact.to_owned())
    }

    async fn complete_turn(
        &self,
        instructions: &str,
        input: &str,
        tools: &[ToolDefinition],
        _: Option<&mut TextDelta<'_>>,
    ) -> Result<String> {
        assert!(instructions.contains("instead of an em dash in your own prose"));
        assert!(instructions.contains("Preserve exact wording and punctuation"));
        assert!(tools.iter().any(|tool| tool.name == "browse"));
        let mut content: Value = serde_json::from_str(input)?;
        assert_eq!(content["latest_request"], QUESTION);
        // Compare without text tool descriptions, which OpenRouter deduplicates on the wire.
        content.as_object_mut().unwrap().remove("tools");
        self.calls
            .lock()
            .unwrap()
            .push((false, instructions.len() + content.to_string().len()));
        Ok(json!({"kind":"reply","text":ANSWER}).to_string())
    }
}

fn model(response: &'static str) -> Arc<QuickFixture> {
    Arc::new(QuickFixture {
        calls: Mutex::new(Vec::new()),
        compact: response,
    })
}

async fn previous_conversation(db: &SqlitePool) {
    message(db, 1, "1", "Summarize the older discussion.").await;
    let previous = store::claim(db).await.unwrap().unwrap();
    store::append(
        db,
        &previous.id,
        "assistant",
        &"Earlier unrelated detail. ".repeat(80),
    )
    .await
    .unwrap();
    store::settle(db, &previous, "idle", None).await.unwrap();
}

#[tokio::test]
async fn a_private_standalone_question_uses_one_smaller_call_for_the_same_answer() {
    let mut bytes = Vec::new();
    for telegram in [false, true] {
        let db = fixture().await;
        sqlx::query(
            "UPDATE users SET display_name='Private User',chief_name='Private Chief' WHERE id='1'",
        )
        .execute(&db)
        .await
        .unwrap();
        let fixture = model(r#"{"kind":"reply","text":"Yeat is een Amerikaanse rapper."}"#);
        let app = app_with_model(&db, fixture.clone());
        previous_conversation(&db).await;
        if telegram {
            store::ingest_telegram(&db, "1", 2, "1", QUESTION, 27)
                .await
                .unwrap();
        } else {
            message(&db, 2, "1", QUESTION).await;
        }
        let task = store::claim(&db).await.unwrap().unwrap();

        runtime::work(&app, &task).await.unwrap();

        assert_eq!(
            store::claim_outbox(&db).await.unwrap().unwrap().body,
            ANSWER
        );
        assert!(store::claim_outbox(&db).await.unwrap().is_none());
        let calls = fixture.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, telegram);
        bytes.push(calls[0].1);
    }
    assert!(bytes[1] * 10 <= bytes[0] * 7, "prompt bytes: {bytes:?}");
}

#[tokio::test]
async fn a_deferred_question_and_an_unregistered_tool_proposal_use_the_normal_broker() {
    for response in [
        r#"{"kind":"defer"}"#,
        r#"{"kind":"tool","name":"set_goal","args":{"text":"should not run"}}"#,
    ] {
        let db = fixture().await;
        let fixture = model(response);
        let app = app_with_model(&db, fixture.clone());
        store::ingest_telegram(&db, "1", 1, "1", QUESTION, 27)
            .await
            .unwrap();
        let task = store::claim(&db).await.unwrap().unwrap();

        runtime::work(&app, &task).await.unwrap();

        assert_eq!(
            store::claim_outbox(&db).await.unwrap().unwrap().body,
            ANSWER
        );
        assert_eq!(task_for(&db, &task.id).await.goal, task.goal);
        let calls = fixture.calls.lock().unwrap();
        assert_eq!(
            calls.iter().map(|call| call.0).collect::<Vec<_>>(),
            [true, false]
        );
    }
}

#[tokio::test]
async fn a_quoted_question_always_keeps_its_reply_context() {
    let db = fixture().await;
    let fixture = model("unexpected brief call");
    let app = app_with_model(&db, fixture.clone());
    store::ingest_telegram_reply(
        &db,
        "1",
        1,
        "1",
        QUESTION,
        27,
        Some(store::ReplyContext {
            message_id: 20,
            excerpt: Some("Context from the original message.".into()),
            from_bot: false,
        }),
    )
    .await
    .unwrap();
    let task = store::claim(&db).await.unwrap().unwrap();

    runtime::work(&app, &task).await.unwrap();

    let item = store::claim_outbox(&db).await.unwrap().unwrap();
    assert_eq!(item.body, ANSWER);
    assert_eq!(item.reply_to_message_id, Some(27));
    assert_eq!(fixture.calls.lock().unwrap()[0].0, false);
}

#[tokio::test]
async fn a_compact_provider_timeout_uses_the_configured_backup_without_retrying_the_primary() {
    let db = fixture().await;
    let primary = model("timeout");
    let backup = model("unexpected brief call");
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
    store::ingest_telegram(&db, "1", 1, "1", QUESTION, 27)
        .await
        .unwrap();
    let task = store::claim(&db).await.unwrap().unwrap();

    runtime::work(&app, &task).await.unwrap();

    let notice = store::claim_outbox(&db).await.unwrap().unwrap();
    assert!(notice.body.contains("test-backup"));
    assert_eq!(
        store::claim_outbox(&db).await.unwrap().unwrap().body,
        ANSWER
    );
    let primary_calls = primary.calls.lock().unwrap();
    let backup_calls = backup.calls.lock().unwrap();
    assert_eq!(primary_calls.len(), 1);
    assert!(primary_calls[0].0);
    assert_eq!(backup_calls.len(), 1);
    assert!(!backup_calls[0].0);
}

#[tokio::test]
async fn an_explicit_dutch_task_overview_needs_zero_model_calls_in_an_existing_chat() {
    let db = fixture().await;
    let model = model("unexpected brief call");
    let app = app_with_model(&db, model.clone());
    previous_conversation(&db).await;
    sqlx::query(
        "INSERT INTO tasks(id,user,title,state,updated) VALUES
        ('one','1','Prepare fixture slides','waiting',10),
        ('done','1','Completed fixture report','completed',11),
        ('private','2','Another account’s private task','idle',12)",
    )
    .execute(&db)
    .await
    .unwrap();
    store::ingest_telegram(
        &db,
        "1",
        2,
        "1",
        "Thanks, daarnaast, welke taken staan er nog open?",
        27,
    )
    .await
    .unwrap();
    let task = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    let reply = store::claim_outbox(&db).await.unwrap().unwrap();
    assert!(reply.body.starts_with("Openstaande taken"));
    assert!(
        reply
            .body
            .contains("Prepare fixture slides · Heeft je aandacht nodig")
    );
    assert!(!reply.body.contains("Completed fixture report"));
    assert!(!reply.body.contains("Another account"));
    assert!(model.calls.lock().unwrap().is_empty());
}
