use super::*;
use crate::{
    forms::{Field, FormSpec, WebsiteForm},
    model::ModelProvider,
    security::now,
    store::Task,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use sqlx::sqlite::SqlitePoolOptions;
use std::collections::BTreeMap;
use tokio::sync::Notify;

async fn fixture() -> SqlitePool {
    let db = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!().run(&db).await.unwrap();
    for user in ["1", "2"] {
        store::seed(&db, user).await.unwrap();
        sqlx::query("UPDATE users SET provider='openrouter',model='test/model' WHERE id=?")
            .bind(user)
            .execute(&db)
            .await
            .unwrap();
    }
    db
}

fn app_with_model(db: &SqlitePool, rust_model: Arc<dyn ModelProvider>) -> App {
    let vault = security::Vault::new(&"f0".repeat(32)).unwrap();
    let origin = url::Url::parse("https://tinybot.example").unwrap();
    App {
        db: db.clone(),
        vault: vault.clone(),
        token: "unused".into(),
        origin: origin.origin().ascii_serialization(),
        models: model::Models::new(db.clone(), vault.clone(), "codex".into(), None, None)
            .with_fixture(rust_model),
        mcp: mcp::Mcp::new(db.clone(), vault, origin),
        forms: forms::Forms::default(),
        workers: workers::Workers {
            browser: "unused".into(),
            python: "unused".into(),
        },
        tools: runtime::tools(),
    }
}

async fn message(db: &SqlitePool, id: i64, user: &str, text: &str) {
    store::ingest(db, "1", "test", id, user, text)
        .await
        .unwrap();
}

async fn task_for(db: &SqlitePool, id: &str) -> Task {
    sqlx::query_as("SELECT * FROM tasks WHERE id=?")
        .bind(id)
        .fetch_one(db)
        .await
        .unwrap()
}

async fn pending_id(db: &SqlitePool, task: &Task, kind: &str) -> String {
    sqlx::query_scalar("SELECT id FROM pending WHERE task=? AND kind=? AND state='waiting'")
        .bind(&task.id)
        .bind(kind)
        .fetch_one(db)
        .await
        .unwrap()
}

struct WidgetChief;

#[async_trait]
impl ModelProvider for WidgetChief {
    async fn complete(&self, system: &str, input: &str) -> Result<String> {
        assert!(system.starts_with("You are TinyBot."));
        let context: Value = serde_json::from_str(input)?;
        let history = context["history"].as_array().unwrap();
        let last = history.last().unwrap()["text"].as_str().unwrap();
        let step = if last == "Give me the menu" {
            json!({"kind":"reply","text":"1. Search online\n2. Try a simple widget"})
        } else if last == "2" {
            // The model can recover from an invalid field without blocking the whole turn.
            json!({"kind":"tool","name":"request_input","args":{
                "purpose":"Try a simple widget",
                "fields":[{"name":"topic","label":"Pick a topic","type":"select"}]
            }})
        } else if last.starts_with("The request_input tool did not complete.") {
            json!({"kind":"tool","name":"request_input","args":{
                "purpose":"Try a simple widget",
                "fields":[{"name":"topic","label":"Pick a topic","type":"select","required":true,
                           "options":["Travel","Cooking"]}]
            }})
        } else {
            assert_eq!(last, r#"My answers to your form: {"topic":"Cooking"}"#);
            json!({"kind":"reply","text":"You picked Cooking."})
        };
        Ok(step.to_string())
    }
}

#[tokio::test]
async fn replying_to_a_menu_opens_an_answerable_choice_without_a_second_model() {
    let db = fixture().await;
    sqlx::query("UPDATE users SET provider='codex',model='test-codex' WHERE id='1'")
        .execute(&db)
        .await
        .unwrap();
    let app = app_with_model(&db, Arc::new(WidgetChief));
    message(&db, 1, "1", "Give me the menu").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    let menu = store::claim_outbox(&db).await.unwrap().unwrap();
    assert!(menu.body.contains("2. Try a simple widget"));

    message(&db, 2, "1", "2").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    assert_eq!(task_for(&db, &task.id).await.state, "waiting");
    let pending = pending_id(&db, &task, "elicitation").await;
    let widget = store::claim_outbox(&db).await.unwrap().unwrap();
    assert_eq!(widget.button.as_deref(), Some("mini"));
    assert!(widget.body.starts_with("Tap Answer"));
    assert!(forms::widget_choice(&db, "2", &pending).await.is_err());
    let choice = forms::widget_choice(&db, "1", &pending).await.unwrap();
    assert_eq!(choice["options"], json!(["Travel", "Cooking"]));
    assert!(forms::answer_choice(&db, "2", &pending, 1).await.is_err());
    assert!(forms::answer_choice(&db, "1", &pending, 2).await.is_err());
    forms::answer_choice(&db, "1", &pending, 1).await.unwrap();
    assert!(forms::answer_choice(&db, "1", &pending, 1).await.is_err());

    let resumed = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(resumed.id, task.id);
    runtime::work(&app, &resumed).await.unwrap();
    let reply = store::claim_outbox(&db).await.unwrap().unwrap();
    assert_eq!(reply.body, "You picked Cooking.");
}

#[tokio::test]
async fn durable_steering_invalidates_an_approval_and_deduplicates_events() {
    let db = fixture().await;
    message(&db, 10, "1", "original instruction").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    store::pause(
        &db,
        &task,
        "approve",
        &json!({"call":"original"}),
        "Approve?",
        None,
        900,
        "approval",
    )
    .await
    .unwrap();
    message(&db, 11, "1", "change direction").await;
    message(&db, 11, "1", "duplicate delivery").await;
    assert!(store::respond(&db, "1", "approval", true).await.is_err());
    let next = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(next.id, task.id);
    assert_eq!(next.revision, task.revision + 1);
    assert_eq!(
        store::history(&db, &task.id)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.2)
            .collect::<Vec<_>>(),
        ["original instruction", "change direction"]
    );
}

#[tokio::test]
async fn approval_expiry_replay_and_restart_preserve_the_external_checkpoint() {
    let db = fixture().await;
    message(&db, 10, "1", "update a record").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    store::pause(
        &db,
        &task,
        "approve",
        &json!({"record":"A"}),
        "Approve?",
        None,
        -1,
        "expired",
    )
    .await
    .unwrap();
    assert!(store::respond(&db, "1", "expired", true).await.is_err());
    store::expire(&db).await.unwrap();
    let task = store::claim(&db).await.unwrap().unwrap();
    store::pause(
        &db,
        &task,
        "approve",
        &json!({"record":"B"}),
        "Approve?",
        None,
        900,
        "live",
    )
    .await
    .unwrap();
    assert!(store::respond(&db, "2", "live", true).await.is_err());
    store::respond(&db, "1", "live", true).await.unwrap();
    assert!(store::respond(&db, "1", "live", true).await.is_err());
    let task = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(
        store::claim_approval(&db, &task).await.unwrap().unwrap().id,
        "live"
    );
    store::recover(&db).await.unwrap();
    assert_eq!(task_for(&db, &task.id).await.state, "uncertain");
    assert!(store::claim(&db).await.unwrap().is_none());
    message(&db, 11, "1", "please continue").await;
    assert!(store::claim(&db).await.unwrap().is_none());
    message(&db, 12, "1", "/reconcile").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    assert!(store::claim_approval(&db, &task).await.unwrap().is_none());
}

#[tokio::test]
async fn invitation_and_record_revocation_prevent_shared_result_delivery() {
    let db = fixture().await;
    sqlx::query(
        "INSERT INTO agents VALUES('agent','1','Reader','Read shared notes.','[\"call_mcp\"]')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO invitations VALUES(?,'1','agent',?)")
        .bind(security::hash("one-use"))
        .bind(now() + 900)
        .execute(&db)
        .await
        .unwrap();
    message(&db, 20, "2", "/start one-use").await;
    message(&db, 21, "3", "/start one-use").await;
    assert!(store::has_access(&db, "2", Some("agent")).await.unwrap());
    assert!(!store::has_access(&db, "3", Some("agent")).await.unwrap());
    message(&db, 22, "2", "/use agent").await;
    message(&db, 23, "2", "What is in this record?").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(task.user, "2");
    store::settle(&db, &task, "idle", Some("old protected result"))
        .await
        .unwrap();
    sqlx::query("INSERT INTO connectors(id,owner,name,endpoint,auth,credential) VALUES('connector','1','Example','https://example.org/mcp','api',NULL)")
        .execute(&db).await.unwrap();
    sqlx::query("INSERT INTO connector_grants VALUES('agent','connector','notes','{}')")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM connector_grants WHERE agent='agent'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(task_for(&db, &task.id).await.state, "revoked");
    let mut found = false;
    while let Some(item) = store::claim_outbox(&db).await.unwrap() {
        if item.body == "old protected result" {
            found = true;
            assert!(!store::can_deliver(&db, &item).await.unwrap());
        }
    }
    assert!(found);
    sqlx::query("DELETE FROM grants WHERE agent='agent' AND member='2'")
        .execute(&db)
        .await
        .unwrap();
    assert!(!store::has_access(&db, "2", Some("agent")).await.unwrap());
}

fn website_form() -> WebsiteForm {
    WebsiteForm {
        page: "https://forms.example/".into(),
        destination: "https://forms.example/submit".into(),
        fingerprint: "pinned".into(),
        index: 0,
        fields: vec![Field {
            name: "contact".into(),
            label: "Contact email".into(),
            kind: "email".into(),
            required: true,
            options: Vec::new(),
            private: true,
            hint: None,
        }],
    }
}

#[tokio::test]
async fn mini_app_form_keeps_approved_values_in_memory_and_scopes_the_request() {
    let db = fixture().await;
    message(&db, 30, "1", "Fill out my form").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    let forms = forms::Forms::default();
    let website = website_form();
    assert!(
        FormSpec { fields: Vec::new() }
            .match_website(&website)
            .is_err()
    );
    let mut changed = website.fields.clone();
    changed[0].kind = "text".into();
    assert!(
        FormSpec { fields: changed }
            .match_website(&website)
            .is_err()
    );
    forms.request(&db, &task, website).await.unwrap();
    let request = pending_id(&db, &task, "form").await;
    let values = BTreeMap::from([("contact".into(), "private@example.net".into())]);
    assert!(
        forms
            .receive(&db, "2", &request, values.clone())
            .await
            .is_err()
    );
    assert!(
        forms
            .receive(&db, "1", &request, BTreeMap::new())
            .await
            .is_err()
    );
    forms
        .receive(&db, "1", &request, values.clone())
        .await
        .unwrap();
    assert!(forms.receive(&db, "1", &request, values).await.is_err());
    let approval = pending_id(&db, &task, "form-submit").await;
    let persisted:Vec<String>=sqlx::query_scalar("SELECT payload FROM pending UNION ALL SELECT body FROM events UNION ALL SELECT body FROM outbox")
        .fetch_all(&db).await.unwrap();
    assert!(
        persisted
            .iter()
            .all(|row| !row.contains("private@example.net"))
    );
    assert!(persisted.iter().any(|row| row.contains("••••")));
    store::respond(&db, "1", &approval, true).await.unwrap();
    let task = store::claim(&db).await.unwrap().unwrap();
    let approved = store::claim_approval(&db, &task).await.unwrap().unwrap();
    let handoff = forms.submission(&approved.id).await.unwrap();
    assert_eq!(handoff["form"]["fingerprint"], "pinned");
    assert_eq!(handoff["values"]["contact"], "private@example.net");
    assert!(forms.submission(&approved.id).await.is_err());
}

#[tokio::test]
async fn restart_drops_unsubmitted_private_inputs_and_resumes_the_task() {
    let db = fixture().await;
    message(&db, 50, "1", "Fill out my form").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    forms::Forms::default()
        .request(&db, &task, website_form())
        .await
        .unwrap();
    store::recover(&db).await.unwrap();
    assert_eq!(task_for(&db, &task.id).await.state, "queued");
    assert!(
        store::history(&db, &task.id)
            .await
            .unwrap()
            .last()
            .unwrap()
            .2
            .contains("expired")
    );
}

#[tokio::test]
async fn compaction_is_atomic_and_preserves_new_and_unprocessed_context() {
    let db = fixture().await;
    message(&db, 40, "1", "initial instruction").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    for index in 0..35 {
        store::append(&db, &task.id, "tool", &format!("record {index}"))
            .await
            .unwrap();
    }
    let records = store::history(&db, &task.id).await.unwrap();
    let batch = store::compaction_batch(&db, &task.id, records[30].0)
        .await
        .unwrap();
    assert_eq!(batch.len(), 28);
    store::compact(&db, &task, batch.last().unwrap().0, "persisted summary")
        .await
        .unwrap();
    message(&db, 41, "1", "latest change").await;
    assert!(
        store::compact(&db, &task, records[30].0, "stale summary")
            .await
            .unwrap()
            .is_none()
    );
    store::recover(&db).await.unwrap();
    let task = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(task.summary, "persisted summary");
    let remaining = store::history(&db, &task.id).await.unwrap();
    assert_eq!(remaining.first().unwrap().0, records[28].0);
    assert_eq!(remaining.last().unwrap().2, "latest change");
}

struct ScheduledChief;

#[async_trait]
impl ModelProvider for ScheduledChief {
    async fn complete(&self, _system: &str, input: &str) -> Result<String> {
        let input: Value = serde_json::from_str(input)?;
        let first = &input["history"][0];
        assert_eq!(first["role"], "user");
        assert!(
            first["text"]
                .as_str()
                .unwrap()
                .ends_with("Check the report")
        );
        assert!(input["agent_role"].is_null());
        let response = if input["goal"] == "Remind the user to check the report" {
            json!({"kind":"reply","text":"Time to check the report."})
        } else {
            json!({"kind":"tool","name":"set_goal","args":{"text":"Remind the user to check the report"}})
        };
        Ok(response.to_string())
    }
}

#[tokio::test]
async fn due_reminder_runs_the_chief_and_delivers_its_reply() {
    let db = fixture().await;
    let time = now();
    sqlx::query(
        "INSERT INTO schedules(id,owner,title,input,kind,time_zone,next_run)
        VALUES('legacy-reminder','2','Report','Check the report','reminder','UTC',?)",
    )
    .bind(time)
    .execute(&db)
    .await
    .unwrap();
    schedules::dispatch(&db, time).await.unwrap();
    assert!(store::claim_outbox(&db).await.unwrap().is_none());
    let task = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(task.user, "2");
    let app = app_with_model(&db, Arc::new(ScheduledChief));
    runtime::work(&app, &task).await.unwrap();
    let reply = store::claim_outbox(&db).await.unwrap().unwrap();
    assert_eq!(reply.user, "2");
    assert_eq!(reply.task.as_deref(), Some(task.id.as_str()));
    assert_eq!(reply.body, "Time to check the report.");
}

struct ScriptedChief {
    reached: Notify,
    resume: Notify,
}

#[async_trait]
impl ModelProvider for ScriptedChief {
    async fn complete(&self, system: &str, input: &str) -> Result<String> {
        let input: Value = serde_json::from_str(input)?;
        let history = input["history"].to_string();
        let response = if system.contains("You are a temporary helper") {
            assert!(
                input["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|tool| tool["name"] != "browse")
            );
            json!({"kind":"reply","text":"Please ask the user which quarter to use."})
        } else if history.contains("new direction") && !history.contains("helper_reply") {
            json!({"kind":"tool","name":"delegate","args":{"brief":"Help determine the requested quarter."}})
        } else if history.contains("helper_reply") {
            json!({"kind":"reply","text":"Which quarter should I use?"})
        } else {
            self.reached.notify_one();
            self.resume.notified().await;
            json!({"kind":"reply","text":"superseded response"})
        };
        Ok(response.to_string())
    }
}

#[tokio::test]
async fn chief_uses_live_steering_and_escalates_the_helpers_question() {
    let db = fixture().await;
    sqlx::query("INSERT INTO agents VALUES('reader','1','Reader','Help with task choices.','[\"delegate\"]')")
        .execute(&db).await.unwrap();
    message(&db, 60, "1", "/use reader").await;
    message(&db, 61, "1", "original").await;
    let rust_model = Arc::new(ScriptedChief {
        reached: Notify::new(),
        resume: Notify::new(),
    });
    let app = Arc::new(app_with_model(&db, rust_model.clone()));
    let task = store::claim(&db).await.unwrap().unwrap();
    let first = tokio::spawn({
        let app = app.clone();
        async move {
            runtime::work(&app, &task).await.unwrap();
        }
    });
    tokio::time::timeout(Duration::from_secs(3), rust_model.reached.notified())
        .await
        .unwrap();
    message(&db, 62, "1", "new direction").await;
    rust_model.resume.notify_one();
    first.await.unwrap();
    let task = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    let history = store::history(&db, &task.id).await.unwrap();
    assert!(history.iter().all(|entry| entry.2 != "superseded response"));
    assert_eq!(history.last().unwrap().2, "Which quarter should I use?");
}
