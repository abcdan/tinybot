use super::{app_with_model, fixture, message, pending_id, task_for};
use crate::{
    App,
    model::{ModelProvider, TextDelta, ToolDefinition},
    runtime::{self, Intent, Outcome, Tool, ToolContext},
    store::{self, McpApprovalDecision, McpApprovalScope},
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const ENDPOINT: &str = "stdio:///docs-fixture";
const APPEND_CHANGELOG: &str = "Append a note to Changelog";
const APPEND_RELEASE: &str = "Append a note to Release notes";
const READ_NOTE: &str = "Read the Roadmap note";
const DELETE_NOTE: &str = "Delete the Roadmap note";
const SECRET: &str = "dbk_synthetic_approval_secret";

struct FixtureMcp {
    inspect: Box<dyn Tool>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for FixtureMcp {
    fn name(&self) -> &'static str {
        self.inspect.name()
    }

    fn describe(&self) -> &'static str {
        self.inspect.describe()
    }

    fn input_schema(&self) -> Value {
        self.inspect.input_schema()
    }

    async fn inspect(&self, ctx: &ToolContext<'_>, args: &Value) -> Result<Intent> {
        self.inspect.inspect(ctx, args).await
    }

    async fn execute(&self, _: &ToolContext<'_>, _: Value) -> Result<Outcome> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(Outcome::Continue(json!({"mcp_fixture_completed":true})))
    }
}

struct Chief;

#[async_trait]
impl ModelProvider for Chief {
    async fn complete(&self, system: &str, input: &str) -> Result<String> {
        let input: Value = serde_json::from_str(input)?;
        let response = if system.starts_with("Review this proposed TinyBot tool action") {
            assert_eq!(
                input["external_write"],
                input["operation"] == "MCP tool append_note"
            );
            json!({"verdict":"allow","reason":"The connector belongs to the user."})
        } else {
            assert!(system.starts_with("Describe what this exact MCP call would do"));
            assert!(!input.to_string().contains(SECRET));
            let explanation = if input["arguments"]["method"] == "DELETE" {
                "The connector would delete the Roadmap note."
            } else {
                assert_eq!(input["arguments"]["title"], "Changelog");
                "The connector would append a note to Changelog."
            };
            json!({"explanation":explanation})
        };
        Ok(response.to_string())
    }

    async fn complete_turn(
        &self,
        _: &str,
        input: &str,
        tools: &[ToolDefinition],
        _: Option<&mut TextDelta<'_>>,
    ) -> Result<String> {
        assert!(tools.iter().any(|tool| tool.name == "call_mcp"));
        let input: Value = serde_json::from_str(input)?;
        let last = input["history"].as_array().unwrap().last().unwrap();
        let last = last["text"].as_str().unwrap();
        let response = if last.contains("Do not retry it in this turn") {
            json!({"kind":"reply","text":"The denied call was blocked."})
        } else if last.contains("mcp_fixture_completed") {
            json!({"kind":"reply","text":"The MCP call finished."})
        } else {
            let (tool, arguments) = match input["latest_request"].as_str().unwrap() {
                APPEND_CHANGELOG => ("append_note", json!({"title":"Changelog","token":SECRET})),
                APPEND_RELEASE => ("append_note", json!({"title":"Release notes"})),
                READ_NOTE => ("get_note", json!({"title":"Roadmap","method":"GET"})),
                DELETE_NOTE => ("get_note", json!({"title":"Roadmap","method":"DELETE"})),
                request => panic!("unexpected request: {request}"),
            };
            json!({"kind":"tool","name":"call_mcp","args":{
                "connector":"Docs Fixture","tool":tool,"arguments":arguments
            }})
        };
        Ok(response.to_string())
    }
}

async fn connector_fixture() -> (App, SqlitePool, Arc<AtomicUsize>) {
    let db = fixture().await;
    sqlx::query("INSERT INTO connectors(id,owner,name,endpoint,auth,transport) VALUES('docs-fixture','1','Docs Fixture',?,'public','stdio')")
        .bind(ENDPOINT)
        .execute(&db)
        .await
        .unwrap();
    let mut app = app_with_model(&db, Arc::new(Chief));
    let index = app
        .tools
        .iter()
        .position(|tool| tool.name() == "call_mcp")
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let inspect = app.tools.remove(index);
    app.tools.push(Box::new(FixtureMcp {
        inspect,
        calls: calls.clone(),
    }));
    (app, db, calls)
}

#[tokio::test]
async fn chief_explains_mcp_writes_and_honors_deny_and_remembered_approval() {
    let (app, db, calls) = connector_fixture().await;
    message(&db, 1, "1", APPEND_CHANGELOG).await;
    let task = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    let request = pending_id(&db, &task, "approve").await;
    let prompt = store::claim_outbox(&db).await.unwrap().unwrap();
    assert_eq!(
        prompt.button.as_deref(),
        Some(format!("mcp-approval/{request}").as_str())
    );
    assert!(
        prompt
            .body
            .contains("The connector would append a note to Changelog.")
    );
    assert!(prompt.body.contains("Docs Fixture · append_note"));
    assert!(!prompt.body.contains(SECRET));
    assert_eq!(calls.load(Ordering::Relaxed), 0);

    store::respond_mcp(&db, "1", &request, McpApprovalDecision::Deny)
        .await
        .unwrap();
    let denied = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &denied).await.unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert_eq!(
        store::claim_outbox(&db).await.unwrap().unwrap().body,
        "The denied call was blocked."
    );

    message(&db, 2, "1", APPEND_CHANGELOG).await;
    let task = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    let request = pending_id(&db, &task, "approve").await;
    store::respond_mcp(&db, "1", &request, McpApprovalDecision::Always)
        .await
        .unwrap();
    assert_eq!(
        store::list_mcp_approvals(&db, "1").await.unwrap(),
        [McpApprovalScope {
            connector: "docs-fixture".into(),
            tool: "append_note".into(),
            epoch: 0,
        }]
    );
    let approved = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &approved).await.unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    message(&db, 3, "1", APPEND_RELEASE).await;
    let task = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(task_for(&db, &task.id).await.state, "idle");
    assert!(store::list_requests(&db, "1").await.unwrap().is_empty());
}

struct JevRead(AtomicUsize);

#[async_trait]
impl ModelProvider for JevRead {
    async fn complete(&self, system: &str, input: &str) -> Result<String> {
        let input: Value = serde_json::from_str(input)?;
        assert_eq!(system, "Jev MCP read decisions");
        assert_eq!(input["questions"]["read_only"]["type"], "noul");
        assert_eq!(input["state"]["tool"], "get_note");
        assert_eq!(input["state"]["arguments"]["method"], "GET");
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(json!({"read_only":{"type":"noul","noul":0.99}}).to_string())
    }
}

#[tokio::test]
async fn jev_autoapproves_a_verified_get_and_a_delete_still_needs_approval() {
    let (mut app, db, calls) = connector_fixture().await;
    app.mcp = app.mcp.with_read_policy(ENDPOINT, &["get_note"]);
    let jev = Arc::new(JevRead(AtomicUsize::new(0)));
    app.models = app.models.with_memory_decisions(jev.clone());

    message(&db, 1, "1", READ_NOTE).await;
    let task = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(jev.0.load(Ordering::Relaxed), 1);
    assert!(store::list_requests(&db, "1").await.unwrap().is_empty());
    assert_eq!(
        store::claim_outbox(&db).await.unwrap().unwrap().body,
        "The MCP call finished."
    );

    message(&db, 2, "1", DELETE_NOTE).await;
    let task = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    let request = pending_id(&db, &task, "approve").await;
    let prompt = store::claim_outbox(&db).await.unwrap().unwrap();
    assert_eq!(
        prompt.button.as_deref(),
        Some(format!("mcp-approval/{request}").as_str())
    );
    assert!(
        prompt
            .body
            .contains("The connector would delete the Roadmap note.")
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(jev.0.load(Ordering::Relaxed), 1);
}
