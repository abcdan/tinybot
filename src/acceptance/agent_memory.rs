use super::*;

const SHARED: &str = "Everyone prepares reports on Thursdays.";
const CHIEF_PRIVATE: &str = "Chief should draft reports on Wednesdays.";
const HELPER_PRIVATE: &str = "Use the Atlas indexing label.";
const SAVE: &str = "Remember the Thursday report schedule for everyone and the Wednesday draft preference privately.";
const DELEGATE: &str = "Ask Librarian to remember the Atlas indexing label privately, then check the Thursday report schedule.";

struct MemoryChief;

impl MemoryChief {
    fn helper(input: &Value) -> Value {
        assert_eq!(input["worker"], "Librarian");
        assert_eq!(input["memory"], json!([SHARED]));
        let observations = input["observations"].as_array().unwrap();
        match observations.as_slice() {
            [] => json!({"kind":"tool","name":"remember","args":{"text":HELPER_PRIVATE}}),
            [saved] => {
                assert_eq!(saved["result"], json!({"saved":true,"scope":"helper"}));
                json!({"kind":"tool","name":"recall_memory","args":{"query":"ATLAS"}})
            }
            [_, recalled] => {
                assert_eq!(recalled["result"], json!({"memories":[HELPER_PRIVATE]}));
                json!({"kind":"reply","text":"I saved and confirmed the private label."})
            }
            _ => panic!("unexpected helper observations"),
        }
    }

    fn chief(input: &Value) -> Value {
        let last = input["history"].as_array().unwrap().last().unwrap();
        let text = last["text"].as_str().unwrap();
        let result = serde_json::from_str::<Value>(text).unwrap_or_default();
        match input["latest_request"].as_str().unwrap() {
            SAVE if text == SAVE => {
                assert_eq!(input["memory"], json!([]));
                json!({"kind":"tool","name":"remember","args":{"scope":"group","text":SHARED}})
            }
            SAVE if result == json!({"saved":true,"scope":"group"}) => {
                json!({"kind":"tool","name":"remember","args":{"text":CHIEF_PRIVATE}})
            }
            SAVE => {
                assert_eq!(result, json!({"saved":true,"scope":"chief"}));
                json!({"kind":"reply","text":"I saved both preferences."})
            }
            DELEGATE if text == DELEGATE => {
                assert_eq!(input["memory"], json!([CHIEF_PRIVATE, SHARED]));
                json!({"kind":"tool","name":"delegate","args":{
                    "agent":"librarian","brief":"The user asks you to remember the Atlas indexing label privately.",
                    "handoff":"I’ll ask Librarian to remember the label and report back."
                }})
            }
            DELEGATE if result["helper_reply"] == "I saved and confirmed the private label." => {
                json!({"kind":"tool","name":"recall_memory","args":{"query":"THURSDAY"}})
            }
            DELEGATE => {
                assert_eq!(result, json!({"memories":[SHARED]}));
                json!({"kind":"reply","text":"Librarian saved its label. Reports are due Thursdays."})
            }
            _ => panic!("unexpected Chief request"),
        }
    }
}

#[async_trait]
impl ModelProvider for MemoryChief {
    async fn complete(&self, _system: &str, _input: &str) -> Result<String> {
        anyhow::bail!("memory actions must use the native tool turn");
    }

    async fn complete_turn(
        &self,
        system: &str,
        input: &str,
        tools: &[ToolDefinition],
        _delta: Option<&mut TextDelta<'_>>,
    ) -> Result<String> {
        for (name, argument) in [("remember", "text"), ("recall_memory", "query")] {
            let tool = tools.iter().find(|tool| tool.name == name).unwrap();
            assert!(tool.input_schema["properties"][argument].is_object());
        }
        let input: Value = serde_json::from_str(input)?;
        let response = if system.starts_with("You are a temporary helper") {
            Self::helper(&input)
        } else {
            Self::chief(&input)
        };
        Ok(response.to_string())
    }
}

#[tokio::test]
async fn chief_and_helper_use_durable_memory_tools_across_conversations() {
    let db = fixture().await;
    sqlx::query(
        "INSERT INTO agents VALUES('librarian','1','Librarian','Maintain the project index.','[]')",
    )
    .execute(&db)
    .await
    .unwrap();
    let app = app_with_model(&db, Arc::new(MemoryChief));

    message(&db, 1, "1", SAVE).await;
    let first = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &first).await.unwrap();
    let saved = store::claim_outbox(&db).await.unwrap().unwrap();
    assert_eq!(saved.body, "I saved both preferences.");

    message(&db, 2, "1", "/new").await;
    message(&db, 3, "1", DELEGATE).await;
    let second = store::claim(&db).await.unwrap().unwrap();
    assert_ne!(first.id, second.id);
    runtime::work(&app, &second).await.unwrap();

    let rows: Vec<(String, Option<String>, String)> = sqlx::query_as(
        "SELECT scope,agent,body FROM agent_memory WHERE owner='1' ORDER BY sequence",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            ("group".into(), None, SHARED.into()),
            ("chief".into(), None, CHIEF_PRIVATE.into()),
            (
                "helper".into(),
                Some("librarian".into()),
                HELPER_PRIVATE.into()
            ),
        ]
    );
    let history = store::history(&db, &second.id).await.unwrap();
    assert_eq!(
        history.last().unwrap().2,
        "Librarian saved its label. Reports are due Thursdays."
    );
}
use super::signed_mini_init;
use crate::web;
#[tokio::test]
async fn signed_memory_settings_are_scoped_to_the_owner() {
    let db = super::fixture().await;
    let app = Arc::new(super::app_with_model(&db, Arc::new(super::WidgetChief)));
    sqlx::query("INSERT INTO agents VALUES('writer','1','Writer','Help write notes.','[]')")
        .execute(&db)
        .await
        .unwrap();
    let owner = signed_mini_init(&app, 1);
    let guest = signed_mini_init(&app, 2);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/api/mini", listener.local_addr().unwrap());
    let origin = app.origin.clone();
    let server =
        tokio::spawn(async move { axum::serve(listener, web::router(app)).await.unwrap() });
    let client = reqwest::Client::new();
    let post = |init: &str, action: &str, data: Value| {
        client
            .post(&endpoint)
            .header("origin", &origin)
            .json(&json!({"init":init,"action":action,"data":data}))
            .send()
    };

    let helper = json!({"scope":"agent","agent":"writer","body":"Use plain words."});
    assert_eq!(
        post(&owner, "save-memory", helper.clone())
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        post(&guest, "save-memory", helper.clone())
            .await
            .unwrap()
            .status(),
        400
    );
    let rows: Value = post(&owner, "list-memories", helper.clone())
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rows["entries"][0]["scope"], "agent");
    assert_eq!(rows["entries"][0]["body"], "Use plain words.");
    let id = rows["entries"][0]["id"].as_str().unwrap();
    let foreign: Value = post(&guest, "list-memories", helper.clone())
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(foreign["entries"], json!([]));
    post(&guest, "delete-memory", json!({"id":id}))
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let unchanged: Value = post(&owner, "list-memories", helper.clone())
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(unchanged["entries"], rows["entries"]);
    post(&owner, "delete-memory", json!({"id":id}))
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let cleared: Value = post(&owner, "list-memories", helper)
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cleared["entries"], json!([]));
    server.abort();
}
