use super::*;
use crate::{
    forms::{Field, FormSpec, WebsiteForm},
    model::{ModelProvider, TextDelta, ToolDefinition},
    security::now,
    store::Task,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use sqlx::sqlite::SqlitePoolOptions;
use std::collections::BTreeMap;
use tokio::sync::Notify;

mod agent_memory;
mod anything_mcp;
mod attachments;
mod audit_log;
mod browser_settings;
#[cfg(unix)]
mod browsing;
#[cfg(unix)]
mod calendar;
mod checkpoints;
mod completion_chat;
mod docblock_saves;
mod latency;
mod mail_settings;
mod mail_workflow;
mod mcp_approvals;
mod model_backup;
mod model_settings;
mod output_limits;
mod projects;
mod quick_chat;
mod routines;
mod self_settings;
mod task_artifacts;
mod task_workbench;
mod telegram_media;
mod telegram_workflows;
mod tool_budget;
mod usage_insights;

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
        task_ready: Default::default(),
        delivery_ready: Default::default(),
        vault: vault.clone(),
        token: "unused".into(),
        operator: "1".into(),
        origin: origin.origin().ascii_serialization(),
        anything_mcp_origin: None,
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

async fn expect_no_immediate_telegram_ack(db: &SqlitePool) {
    assert!(store::claim_outbox(db).await.unwrap().is_none());
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

fn signed_mini_init(app: &App, id: u64) -> String {
    signed_mini_init_at(app, id, now())
}

fn signed_mini_init_at(app: &App, id: u64, date: i64) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let date = date.to_string();
    let user = json!({"id":id}).to_string();
    let mut key = Hmac::<Sha256>::new_from_slice(b"WebAppData").unwrap();
    key.update(app.token.as_bytes());
    let mut signature = Hmac::<Sha256>::new_from_slice(&key.finalize().into_bytes()).unwrap();
    signature.update(format!("auth_date={date}\nuser={user}").as_bytes());
    url::form_urlencoded::Serializer::new(String::new())
        .append_pair("auth_date", &date)
        .append_pair("user", &user)
        .append_pair("hash", &hex::encode(signature.finalize().into_bytes()))
        .finish()
}

struct WidgetChief;

#[async_trait]
impl ModelProvider for WidgetChief {
    async fn complete_turn(
        &self,
        system: &str,
        input: &str,
        tools: &[ToolDefinition],
        _delta: Option<&mut TextDelta<'_>>,
    ) -> Result<String> {
        for (name, field) in [
            ("request_input", "fields"),
            ("send_reply", "suggested_replies"),
            ("send_card", "text"),
            ("react_to_message", "emoji"),
            ("browse", "url"),
            ("search_web", "query"),
        ] {
            let tool = tools.iter().find(|tool| tool.name == name).unwrap();
            assert!(tool.input_schema["properties"][field].is_object());
        }
        self.complete(system, input).await
    }

    async fn complete(&self, system: &str, input: &str) -> Result<String> {
        assert!(system.starts_with("You are TinyBot."));
        let context: Value = serde_json::from_str(input)?;
        let history = context["history"].as_array().unwrap();
        let last = history.last().unwrap()["text"].as_str().unwrap();
        let step = if last == "Can you make a widget that says hello?" {
            assert!(system.contains("Call send_card"));
            json!({"kind":"tool","name":"send_card","args":{"text":"Hello!","suggested_replies":["Say it in Dutch"]}})
        } else if last == "Create a Mini App form with one choice." {
            assert!(system.contains("surface=mini_app"));
            json!({"kind":"tool","name":"request_input","args":{
                "purpose":"Pick a topic",
                "surface":"mini_app",
                "fields":[{"name":"topic","label":"Pick a topic","type":"select","options":["Travel","Cooking"]}]
            }})
        } else if last == "Create a Mini App form for my project." {
            json!({"kind":"tool","name":"request_input","args":{
                "purpose":"Collect project details",
                "surface":"mini_app",
                "fields":[
                    {"name":"project","label":"Project name","type":"text","required":true},
                    {"name":"priority","label":"Priority","type":"select","required":true,"options":["Normal","High"]}
                ]
            }})
        } else if last
            == r#"My answers to your form: {"priority":"High","project":"Weekend plans"}"#
        {
            json!({"kind":"reply","text":"I saved Weekend plans with high priority."})
        } else if last == "Give me the menu" {
            json!({"kind":"tool","name":"send_reply","args":{
                "text":"1. Search online\n2. Try a simple widget",
                "suggested_replies":["Search online", "Try a simple widget"]
            }})
        } else if last == "React with a thumbs-up." {
            json!({"kind":"tool","name":"react_to_message","args":{"emoji":"👍"}})
        } else if last == r#"{"queued":true}"# {
            json!({"kind":"reply","text":""})
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
    let options = store::suggested_replies(&db, menu.id).await.unwrap();
    assert_eq!(options[1].body, "Try a simple widget");

    message(&db, 2, "1", "2").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    assert_eq!(task_for(&db, &task.id).await.state, "waiting");
    let pending = pending_id(&db, &task, "elicitation").await;
    let widget = store::claim_outbox(&db).await.unwrap().unwrap();
    assert_eq!(widget.button, Some(format!("choice/{pending}")));
    assert_eq!(widget.body, "Pick a topic");
    assert!(forms::widget_choice(&db, "2", &pending).await.is_err());
    let choice = forms::widget_choice(&db, "1", &pending).await.unwrap();
    assert_eq!(choice.options, ["Travel", "Cooking"]);
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
async fn the_agent_can_react_without_sending_a_text_reply() {
    let db = fixture().await;
    let app = app_with_model(&db, Arc::new(WidgetChief));
    store::ingest_telegram(&db, "1", 1, "1", "React with a thumbs-up.", 42)
        .await
        .unwrap();
    expect_no_immediate_telegram_ack(&db).await;
    let task = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    let reaction = store::claim_outbox(&db).await.unwrap().unwrap();
    assert_eq!(reaction.body, "👍");
    assert!(reaction.reaction_event.is_some());
    assert!(store::claim_outbox(&db).await.unwrap().is_none());
}

#[tokio::test]
async fn a_widget_reply_queues_a_native_widget_without_a_form_request() {
    let db = fixture().await;
    let app = app_with_model(&db, Arc::new(WidgetChief));
    message(&db, 1, "1", "Can you make a widget that says hello?").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    let widget = store::claim_outbox(&db).await.unwrap().unwrap();
    assert_eq!(widget.body, "Hello!");
    let content: crate::channel::Card =
        serde_json::from_str(widget.widget.as_deref().unwrap()).unwrap();
    assert!(content.html().contains("<p>Hello!</p>"));
    let replies = store::suggested_replies(&db, widget.id).await.unwrap();
    assert_eq!(replies[0].body, "Say it in Dutch");
    assert!(store::list_requests(&db, "1").await.unwrap().is_empty());
    assert_eq!(task_for(&db, &task.id).await.state, "idle");
}

#[tokio::test]
async fn an_explicit_mini_app_form_keeps_a_single_select_in_the_mini_app() {
    let db = fixture().await;
    let app = app_with_model(&db, Arc::new(WidgetChief));
    message(&db, 1, "1", "Create a Mini App form with one choice.").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    let id = pending_id(&db, &task, "elicitation").await;
    let delivery = store::claim_outbox(&db).await.unwrap().unwrap();
    assert_eq!(delivery.button, Some(format!("form/{id}")));
    assert!(forms::widget_choice(&db, "1", &id).await.is_err());
    let form = forms::input_request(&db, "1", &id).await.unwrap();
    assert_eq!(
        form["content"]["fields"][0]["options"],
        json!(["Travel", "Cooking"])
    );
}

#[tokio::test]
async fn telegram_mini_app_launches_at_the_origin_and_requires_signed_access() {
    let db = fixture().await;
    let app = Arc::new(app_with_model(&db, Arc::new(WidgetChief)));
    let init = signed_mini_init(&app, 1);
    let expired = signed_mini_init_at(&app, 1, now() - 901);
    let mut tampered = init.clone();
    let last = tampered.pop().unwrap();
    tampered.push(if last == '0' { '1' } else { '0' });
    let unregistered = signed_mini_init(&app, 3);
    let origin = app.origin.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server =
        tokio::spawn(async move { axum::serve(listener, web::router(app)).await.unwrap() });
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    for path in ["/", "/mini?form=example"] {
        let response = client
            .get(format!("{endpoint}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let csp = response.headers()["content-security-policy"]
            .to_str()
            .unwrap();
        assert!(csp.contains("script-src 'self' https://telegram.org"));
        assert!(csp.contains("frame-ancestors 'self' https://web.telegram.org"));
        let body = response.text().await.unwrap();
        assert!(body.contains("https://telegram.org/js/telegram-web-app.js"));
        assert!(body.contains("src=\"/mini.js\""));
    }

    for (init, origin, status) in [
        ("", origin.as_str(), 403),
        (tampered.as_str(), origin.as_str(), 403),
        (expired.as_str(), origin.as_str(), 403),
        (unregistered.as_str(), origin.as_str(), 403),
        (init.as_str(), "https://other.example", 403),
        (init.as_str(), origin.as_str(), 200),
    ] {
        let response = client
            .post(format!("{endpoint}/api/mini"))
            .header("origin", origin)
            .json(&json!({"init":init,"action":"status"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert!(
            response.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'")
        );
        if status == 200 {
            assert_eq!(response.json::<Value>().await.unwrap()["user"], "1");
        }
    }
    server.abort();
}

#[tokio::test]
async fn generated_form_submits_through_the_signed_mini_app_api_and_resumes_the_task() {
    let db = fixture().await;
    let app = Arc::new(app_with_model(&db, Arc::new(WidgetChief)));
    message(&db, 1, "1", "Create a Mini App form for my project.").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    let notice = store::claim_outbox(&db).await.unwrap().unwrap();
    assert_eq!(notice.task.as_deref(), Some(task.id.as_str()));
    let form_id = notice
        .button
        .as_deref()
        .unwrap()
        .strip_prefix("form/")
        .unwrap();
    let owner = signed_mini_init(&app, 1);
    let other = signed_mini_init(&app, 2);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/api/mini", listener.local_addr().unwrap());
    let origin = app.origin.clone();
    let router = web::router(app.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = reqwest::Client::new();
    let post = |init: &str, action: &str, data: Value| {
        client
            .post(&endpoint)
            .header("origin", &origin)
            .json(&json!({"init":init,"action":action,"data":data}))
            .send()
    };

    for (init, status) in [("invalid", 403), (other.as_str(), 404)] {
        assert_eq!(
            post(init, "get-form", json!({"id":form_id}))
                .await
                .unwrap()
                .status(),
            status
        );
    }
    let response = post(&owner, "get-form", json!({"id":form_id}))
        .await
        .unwrap();
    assert!(response.status().is_success());
    let result: Value = response.json().await.unwrap();
    assert_eq!(result["content"]["fields"][0]["type"], "text");
    assert_eq!(result["content"]["fields"][1]["type"], "select");
    assert_eq!(
        post(
            &owner,
            "answer-form",
            json!({"id":form_id,"values":{"project":"Weekend plans"}})
        )
        .await
        .unwrap()
        .status(),
        400,
    );
    let values = json!({"id":form_id,"values":{"project":"Weekend plans","priority":"High"}});
    assert_eq!(
        post(&other, "answer-form", values.clone())
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        post(&owner, "answer-form", values.clone())
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        post(&owner, "answer-form", values).await.unwrap().status(),
        400
    );
    assert!(store::claim_outbox(&db).await.unwrap().is_none());
    let resumed = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(resumed.id, task.id);
    assert_eq!(resumed.revision, task.revision + 1);
    runtime::work(&app, &resumed).await.unwrap();
    let confirmation = store::claim_outbox(&db).await.unwrap().unwrap();
    assert_eq!(
        confirmation.body,
        "I saved Weekend plans with high priority."
    );
    assert_eq!(task_for(&db, &resumed.id).await.state, "idle");
    server.abort();
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
    message(&db, 23, "2", "What is in this record?").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(task.user, "2");
    sqlx::query("UPDATE tasks SET agent='agent' WHERE id=?")
        .bind(&task.id)
        .execute(&db)
        .await
        .unwrap();
    let task = task_for(&db, &task.id).await;
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
        FormSpec {
            fields: Vec::new(),
            mini_app: false
        }
        .match_website(&website)
        .is_err()
    );
    let mut changed = website.fields.clone();
    changed[0].kind = "text".into();
    assert!(
        FormSpec {
            fields: changed,
            mini_app: false
        }
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
    assert_eq!(batch[0].0, records[1].0);
    store::settle(&db, &task, "idle", None).await.unwrap();
    assert!(
        store::compact(&db, &task, batch.last().unwrap().0, "persisted summary")
            .await
            .unwrap()
            .is_some()
    );
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
    assert_eq!(remaining[0].2, "initial instruction");
    assert_eq!(remaining[1].0, records[29].0);
    assert_eq!(remaining.last().unwrap().2, "latest change");
}

#[tokio::test]
async fn a_slow_history_summary_never_delays_the_current_reply() {
    #[derive(Default)]
    struct Model {
        summary_started: Notify,
        release_summary: Notify,
    }

    #[async_trait]
    impl ModelProvider for Model {
        async fn complete(&self, instructions: &str, input: &str) -> Result<String> {
            if instructions.starts_with("Summarize only durable task context") {
                let input: Value = serde_json::from_str(input)?;
                assert!(
                    input["history"][0][2]
                        .as_str()
                        .unwrap()
                        .starts_with("Older")
                );
                self.summary_started.notify_one();
                self.release_summary.notified().await;
                return Ok(json!({"summary": "Earlier decisions are retained."}).to_string());
            }
            let input: Value = serde_json::from_str(input)?;
            assert_eq!(input["latest_request"], "Check the current decision.");
            assert!(input["history"].to_string().contains("Latest decision"));
            Ok(json!({"kind":"reply","text":"Here is the latest decision."}).to_string())
        }
    }

    let db = fixture().await;
    let model = Arc::new(Model::default());
    let app = app_with_model(&db, model.clone());
    message(&db, 1, "1", "Check the current decision.").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    for _ in 0..7 {
        store::append(&db, &task.id, "assistant", &"Older decision. ".repeat(225))
            .await
            .unwrap();
    }
    store::append(&db, &task.id, "assistant", "Latest decision.")
        .await
        .unwrap();

    let limit = std::time::Duration::from_secs(3);
    tokio::time::timeout(limit, runtime::work(&app, &task))
        .await
        .expect("the summary is still waiting for its provider")
        .unwrap();
    let reply = store::claim_outbox(&db).await.unwrap().unwrap();
    assert_eq!(reply.body, "Here is the latest decision.");
    tokio::time::timeout(limit, model.summary_started.notified())
        .await
        .unwrap();
    assert!(task_for(&db, &task.id).await.summary.is_empty());
    assert_eq!(store::history(&db, &task.id).await.unwrap().len(), 10);

    model.release_summary.notify_one();
    tokio::time::timeout(limit, async {
        loop {
            if task_for(&db, &task.id).await.summary == "Earlier decisions are retained." {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let history = store::history(&db, &task.id).await.unwrap();
    assert_eq!(history.len(), 8);
    assert_eq!(history[0].2, "Check the current decision.");
    assert_eq!(history.last().unwrap().2, reply.body);
}

#[tokio::test]
async fn due_legacy_reminder_delivers_the_saved_text_with_no_model() {
    let db = fixture().await;
    let time = now();
    sqlx::query("UPDATE users SET provider=NULL WHERE id='2'")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO schedules(id,owner,title,input,kind,time_zone,next_run)
        VALUES('legacy-reminder','2','Report','Check the report','reminder','UTC',?)",
    )
    .bind(time)
    .execute(&db)
    .await
    .unwrap();
    schedules::dispatch(&db, time).await.unwrap();
    assert!(store::claim(&db).await.unwrap().is_none());
    let reply = store::claim_outbox(&db).await.unwrap().unwrap();
    assert_eq!(reply.user, "2");
    assert_eq!(reply.body, "Check the report");
    let task = task_for(&db, reply.task.as_deref().unwrap()).await;
    assert_eq!(task.state, "completed");
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
            json!({"kind":"tool","name":"delegate","args":{"agent":"reader","brief":"Help determine the requested quarter.","handoff":"I’ll ask Reader which quarter we need and get back to you."}})
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

struct SharedHelperChief;

#[async_trait]
impl ModelProvider for SharedHelperChief {
    async fn complete(&self, prompt: &str, input: &str) -> Result<String> {
        let input: Value = serde_json::from_str(input)?;
        let outcome = if prompt.contains("You are a temporary helper") {
            assert_eq!(input["worker"], "Reader");
            assert_eq!(input["memory"], json!(["Shared fixture note"]));
            json!({"kind":"reply","text":"Here is the shared fixture note."})
        } else if input["history"].to_string().contains("helper_reply") {
            assert_eq!(input["shared_helper_scope"], "reader");
            json!({"kind":"reply","text":"Reader found the shared fixture note."})
        } else {
            assert_eq!(input["saved_agents"][0]["id"], "reader");
            assert!(input["saved_agents"][0]["description"].is_null());
            json!({"kind":"tool","name":"delegate","args":{
                "agent":"reader","brief":"Find the shared note.","handoff":"I’ll ask Reader to find that note and get back to you."
            }})
        };
        Ok(outcome.to_string())
    }
}

#[tokio::test]
async fn chief_binds_a_shared_helper_scope_before_returning_its_result() {
    let db = fixture().await;
    sqlx::query("INSERT INTO agents VALUES('reader','1','Reader','Read shared notes.','[]')")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO grants VALUES('reader','2')")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memory VALUES('published','1','reader','Shared fixture note')")
        .execute(&db)
        .await
        .unwrap();
    let app = app_with_model(&db, Arc::new(SharedHelperChief));
    message(&db, 1, "2", "Ask Reader for the shared note.").await;
    let primary = store::claim(&db).await.unwrap().unwrap();
    assert!(primary.agent.is_none());
    runtime::work(&app, &primary).await.unwrap();
    assert!(store::claim_outbox(&db).await.unwrap().is_none());

    let scoped = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(scoped.id, primary.id);
    assert_eq!(scoped.agent.as_deref(), Some("reader"));
    runtime::work(&app, &scoped).await.unwrap();
    let handoff = store::claim_outbox(&db).await.unwrap().unwrap();
    assert!(handoff.body.starts_with("I’ll ask Reader"));
    let reply = store::claim_outbox(&db).await.unwrap().unwrap();
    assert_eq!(reply.body, "Reader found the shared fixture note.");
    assert!(store::can_deliver(&db, &reply).await.unwrap());
    sqlx::query("DELETE FROM grants WHERE agent='reader' AND member='2'")
        .execute(&db)
        .await
        .unwrap();
    assert!(!store::can_deliver(&db, &reply).await.unwrap());
}

struct ReplyMenuModel;

#[async_trait]
impl ModelProvider for ReplyMenuModel {
    async fn complete(&self, _: &str, _: &str) -> Result<String> {
        Ok(json!({
            "kind":"reply","text":"Which summary would you like?",
            "suggested_replies":["Give me the short version.", "/cancel", 13, "Give me the details."]
        })
        .to_string())
    }
}

#[tokio::test]
async fn tapped_reply_resumes_its_original_task_once() {
    let db = fixture().await;
    let app = app_with_model(&db, Arc::new(ReplyMenuModel));
    message(&db, 101, "1", "Summarize the options.").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    let item = store::claim_outbox(&db).await.unwrap().unwrap();
    assert_eq!(item.body, "Which summary would you like?");
    let options = store::suggested_replies(&db, item.id).await.unwrap();
    assert_eq!(options.len(), 2);
    assert_eq!(options[0].body, "Give me the short version.");
    assert_eq!(options[1].body, "Give me the details.");
    assert!(
        !store::redeem_reply(&db, "2", &options[0].token)
            .await
            .unwrap()
    );

    message(&db, 102, "1", "/new A different task").await;
    let different_task: String = sqlx::query_scalar("SELECT active_task FROM users WHERE id='1'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_ne!(different_task, task.id);

    // A message already accepted by Telegram may have been visible before recovery.
    store::recover(&db).await.unwrap();
    let (first, second) = tokio::join!(
        store::redeem_reply(&db, "1", &options[0].token),
        store::redeem_reply(&db, "1", &options[1].token),
    );
    let first = first.unwrap();
    assert_ne!(first, second.unwrap());
    assert!(
        !store::redeem_reply(&db, "1", &options[0].token)
            .await
            .unwrap()
    );
    assert!(
        !store::redeem_reply(&db, "1", &options[1].token)
            .await
            .unwrap()
    );

    let current = task_for(&db, &task.id).await;
    assert_eq!(current.revision, task.revision + 1);
    assert_eq!(current.state, "queued");
    let active: String = sqlx::query_scalar("SELECT active_task FROM users WHERE id='1'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(active, task.id);
    let events = store::history(&db, &task.id).await.unwrap();
    let (_, role, body) = events.last().unwrap();
    assert_eq!(role, "user");
    assert_eq!(body, &options[if first { 0 } else { 1 }].body);
    assert_eq!(store::history(&db, &different_task).await.unwrap().len(), 1);
}

#[tokio::test]
async fn stale_expired_and_revoked_reply_buttons_cannot_queue_input() {
    let db = fixture().await;
    sqlx::query("INSERT INTO agents(id,owner,name,instructions,tools) VALUES('helper','2','Helper','','[]')")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO grants(agent,member) VALUES('helper','1')")
        .execute(&db)
        .await
        .unwrap();
    let app = app_with_model(&db, Arc::new(ReplyMenuModel));
    message(&db, 104, "1", "Give me options.").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    sqlx::query("UPDATE tasks SET agent='helper' WHERE id=?")
        .bind(&task.id)
        .execute(&db)
        .await
        .unwrap();
    let task = task_for(&db, &task.id).await;
    runtime::work(&app, &task).await.unwrap();
    let item = store::claim_outbox(&db).await.unwrap().unwrap();
    let options = store::suggested_replies(&db, item.id).await.unwrap();
    assert_eq!(options.len(), 2);

    sqlx::query("UPDATE suggested_replies SET expires=0 WHERE token=?")
        .bind(&options[0].token)
        .execute(&db)
        .await
        .unwrap();
    assert!(
        !store::redeem_reply(&db, "1", &options[0].token)
            .await
            .unwrap()
    );
    message(&db, 105, "1", "This follow-up is newer.").await;
    let revised = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &revised).await.unwrap();
    assert!(
        store::suggested_replies(&db, item.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !store::redeem_reply(&db, "1", &options[1].token)
            .await
            .unwrap()
    );

    let latest_item = store::claim_outbox(&db).await.unwrap().unwrap();
    let latest = store::suggested_replies(&db, latest_item.id).await.unwrap();
    assert_eq!(latest.len(), 2);
    sqlx::query("DELETE FROM grants WHERE agent='helper' AND member='1'")
        .execute(&db)
        .await
        .unwrap();
    assert!(
        !store::redeem_reply(&db, "1", &latest[0].token)
            .await
            .unwrap()
    );
}
