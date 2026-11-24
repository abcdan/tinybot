use super::{app_with_model, fixture, signed_mini_init};
use crate::{
    App,
    model::{ModelProvider, TextDelta, ToolDefinition},
    runtime, store, web,
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

struct SettingsChief;

#[async_trait]
impl ModelProvider for SettingsChief {
    async fn complete(&self, _: &str, _: &str) -> Result<String> {
        unreachable!("settings use the normal Chief tool protocol")
    }

    async fn complete_turn(
        &self,
        _: &str,
        input: &str,
        tools: &[ToolDefinition],
        _: Option<&mut TextDelta<'_>>,
    ) -> Result<String> {
        let context: Value = serde_json::from_str(input)?;
        let history = context["history"].as_array().unwrap();
        let last = history.last().unwrap();
        let request = context["latest_request"].as_str().unwrap();
        let has_tool = |name: &str| tools.iter().any(|tool| tool.name == name);
        if request == "Scheduled settings check" {
            assert!(!has_tool("read_settings") && !has_tool("configure_settings"));
            return Ok(json!({"kind":"reply","text":"Scheduled task completed."}).to_string());
        }
        assert!(has_tool("read_settings") && has_tool("configure_settings"));
        match (request, last["role"].as_str()) {
            ("Show my settings", Some("user")) => {
                Ok(json!({"kind":"tool","name":"read_settings","args":{}}).to_string())
            }
            ("Show my settings", Some("tool")) => {
                let settings: Value = serde_json::from_str(last["text"].as_str().unwrap())?;
                let output = settings.to_string();
                assert!(output.contains("Europe/London"));
                assert!(output.contains("Current Chief"));
                assert!(!output.contains("fixture-connector-secret"));
                assert!(!output.contains("private-endpoint"));
                assert!(!output.contains("Other account"));
                Ok(json!({"kind":"reply","text":"Your timezone is Europe/London."}).to_string())
            }
            ("Rename yourself to Atlas and update my timezone", Some("user")) => {
                Ok(json!({"kind":"tool","name":"configure_settings","args":{
                    "fields":["chief_name","time_zone"],
                    "values":{"chief_name":"Atlas"}
                }})
                .to_string())
            }
            ("Rename yourself to Atlas and update my timezone", Some("tool")) => {
                Ok(json!({"kind":"reply","text":"I confirmed your settings request."}).to_string())
            }
            _ => panic!("unexpected settings turn"),
        }
    }
}

struct MiniFixture {
    client: reqwest::Client,
    endpoint: String,
    origin: String,
    owner: String,
    other: String,
    server: tokio::task::JoinHandle<()>,
}

impl MiniFixture {
    async fn start(app: Arc<App>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/api/mini", listener.local_addr().unwrap());
        let origin = app.origin.clone();
        let owner = signed_mini_init(&app, 1);
        let other = signed_mini_init(&app, 2);
        let server = tokio::spawn(async move {
            axum::serve(listener, web::router(app)).await.unwrap();
        });
        Self {
            client: reqwest::Client::new(),
            endpoint,
            origin,
            owner,
            other,
            server,
        }
    }

    async fn post(&self, owner: bool, action: &str, data: Value) -> reqwest::Response {
        self.client.post(&self.endpoint).header("origin",&self.origin)
            .json(&json!({"init":if owner { &self.owner } else { &self.other },"action":action,"data":data}))
            .send().await.unwrap()
    }
}

impl Drop for MiniFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn send(app: &App, update: i64, text: &str) -> store::Task {
    store::ingest_telegram_reply(&app.db, "1", update, "1", text, update, None)
        .await
        .unwrap();
    let task = store::claim(&app.db).await.unwrap().unwrap();
    runtime::work(app, &task).await.unwrap();
    task
}

async fn proposed_form(app: &App, update: i64) -> (String, String) {
    let task = send(
        app,
        update,
        "Rename yourself to Atlas and update my timezone",
    )
    .await;
    let id: String = sqlx::query_scalar(
        "SELECT id FROM pending WHERE task=? AND kind='settings-form' AND state='waiting'",
    )
    .bind(&task.id)
    .fetch_one(&app.db)
    .await
    .unwrap();
    (task.id, id)
}

async fn saved(app: &App) -> (String, String) {
    sqlx::query_as("SELECT chief_name,time_zone FROM users WHERE id='1'")
        .fetch_one(&app.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn chief_reads_redacted_settings_and_applies_only_the_exact_signed_approval() {
    let db = fixture().await;
    let app = Arc::new(app_with_model(&db, Arc::new(SettingsChief)));
    sqlx::query(
        "UPDATE users SET chief_name='Current Chief',time_zone='Europe/London' WHERE id='1'",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO connectors(id,owner,name,endpoint,auth,credential)
        VALUES('owner-fixture','1','My integration','https://fixture.test/private-endpoint','api','fixture-connector-secret'),
              ('other-fixture','2','Other account','https://other.test/mcp','pending',NULL)")
        .execute(&db).await.unwrap();
    send(&app, 10, "Show my settings").await;
    let (task, form) = proposed_form(&app, 11).await;
    let mini = MiniFixture::start(app.clone()).await;
    let get = json!({"id":form});
    assert_eq!(
        mini.post(false, "get-form", get.clone()).await.status(),
        404
    );
    let form_state: Value = mini
        .post(true, "get-form", get.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(form_state["kind"], "settings-form");
    assert_eq!(form_state["content"]["fields"][0]["value"], "Atlas");
    assert_eq!(form_state["content"]["fields"][1]["value"], "Europe/London");
    assert!(!mini.post(true, "approve", get).await.status().is_success());

    let input = json!({"id":form,"values":{"chief_name":"Orion","time_zone":"Europe/Amsterdam"}});
    assert!(
        !mini
            .post(false, "answer-settings-form", input.clone())
            .await
            .status()
            .is_success()
    );
    let mut unexpected = input.clone();
    unexpected["values"]["openrouter_key"] = json!("fixture-credential");
    assert!(
        !mini
            .post(true, "answer-settings-form", unexpected)
            .await
            .status()
            .is_success()
    );
    let review: Value = mini
        .post(true, "answer-settings-form", input.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(review["kind"], "settings-approve");
    let changes = &review["content"]["changes"];
    assert!(
        changes
            .as_array()
            .unwrap()
            .iter()
            .any(|change| { change["before"] == "Current Chief" && change["after"] == "Orion" })
    );
    assert_eq!(
        saved(&app).await,
        ("Current Chief".into(), "Europe/London".into())
    );
    assert!(
        !mini
            .post(true, "answer-settings-form", input)
            .await
            .status()
            .is_success()
    );
    let approve = json!({"id":review["id"]});
    assert!(
        !mini
            .post(false, "approve-settings", approve.clone())
            .await
            .status()
            .is_success()
    );
    assert!(
        !mini
            .post(true, "approve", approve.clone())
            .await
            .status()
            .is_success()
    );
    assert!(
        mini.post(true, "approve-settings", approve.clone())
            .await
            .status()
            .is_success()
    );
    assert!(
        !mini
            .post(true, "approve-settings", approve)
            .await
            .status()
            .is_success()
    );
    assert_eq!(
        saved(&app).await,
        ("Orion".into(), "Europe/Amsterdam".into())
    );
    let audit: Vec<String> = sqlx::query_scalar("SELECT e.detail FROM audit_entries e JOIN audit_sessions s ON s.id=e.session WHERE s.task=?")
        .bind(task).fetch_all(&db).await.unwrap();
    assert!(!audit.join(" ").contains("Orion"));
}

#[tokio::test]
async fn settings_reject_a_stale_review_and_stay_out_of_scheduled_tasks() {
    let db = fixture().await;
    let app = Arc::new(app_with_model(&db, Arc::new(SettingsChief)));
    sqlx::query(
        "UPDATE users SET chief_name='Current Chief',time_zone='Europe/London' WHERE id='1'",
    )
    .execute(&db)
    .await
    .unwrap();
    let (_, form) = proposed_form(&app, 20).await;
    let mini = MiniFixture::start(app.clone()).await;
    let review: Value = mini
        .post(
            true,
            "answer-settings-form",
            json!({
                "id":form,"values":{"chief_name":"Orion","time_zone":"Europe/Amsterdam"}
            }),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        mini.post(true, "time-zone", json!({"zone":"Europe/Paris"}))
            .await
            .status()
            .is_success()
    );
    assert!(
        !mini
            .post(true, "approve-settings", json!({"id":review["id"]}))
            .await
            .status()
            .is_success()
    );
    assert_eq!(
        saved(&app).await,
        ("Current Chief".into(), "Europe/Paris".into())
    );

    store::ingest_telegram_reply(&db, "1", 21, "1", "/new Scheduled settings check", 21, None)
        .await
        .unwrap();
    let task = store::claim(&db).await.unwrap().unwrap();
    sqlx::query("UPDATE tasks SET schedule='fixture' WHERE id=?")
        .bind(&task.id)
        .execute(&db)
        .await
        .unwrap();
    runtime::work(&app, &task).await.unwrap();
}
