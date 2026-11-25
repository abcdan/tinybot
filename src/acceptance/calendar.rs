use super::{app_with_model, fixture, pending_id, signed_mini_init, task_for};
use crate::{
    App,
    model::{ModelProvider, TextDelta, ToolDefinition},
    runtime, security, store, web,
};
use anyhow::Result;
use async_trait::async_trait;
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::net::{TcpListener, UnixListener};

const START: &str = "2027-01-08T15:00:00+01:00";
const END: &str = "2027-01-08T15:30:00+01:00";
const TITLE: &str = "Quarterly review";
const KEY: &str = "calendar_synthetic_key";
const GUEST: &str = "casey@example.test";

struct CalendarChief;

#[async_trait]
impl ModelProvider for CalendarChief {
    async fn complete(&self, system: &str, _: &str) -> Result<String> {
        assert!(system.starts_with("Review this proposed TinyBot tool action"));
        Ok(json!({"verdict":"allow","reason":"Read the owner's selected calendar."}).to_string())
    }

    async fn complete_turn(
        &self,
        _: &str,
        input: &str,
        tools: &[ToolDefinition],
        _: Option<&mut TextDelta<'_>>,
    ) -> Result<String> {
        let input: Value = serde_json::from_str(input)?;
        assert!(tools.iter().any(|tool| tool.name == "draft_calendar_event"));
        assert!(
            tools
                .iter()
                .any(|tool| tool.name == "calendar_availability")
        );
        assert_eq!(input["calendar"]["account"], "work");
        assert_eq!(input["calendar"]["calendar_id"], "team@example.test");
        let last = input["history"].as_array().unwrap().last().unwrap();
        let step = if last["role"] == "user" {
            json!({"kind":"tool","name":"call_mcp","args":{
                "connector":"Team calendar","tool":"create-event","arguments":{"summary":TITLE}
            }})
        } else if last["text"]
            .as_str()
            .unwrap()
            .starts_with("Use draft_calendar_event")
        {
            json!({"kind":"tool","name":"calendar_availability","args":{"start":START,"end":END}})
        } else if last["text"].as_str().unwrap().contains("\"verified\":true") {
            assert!(last["text"].as_str().unwrap().contains("\"busy\":[]"));
            json!({"kind":"tool","name":"draft_calendar_event","args":{
                "title":TITLE,"start":START,"end":END,"attendees":[{"name":"Casey","email":GUEST}],
                "notify_attendees":true
            }})
        } else {
            json!({"kind":"reply","text":""})
        };
        Ok(step.to_string())
    }
}

async fn setup(
    verified: bool,
) -> (
    App,
    Arc<Mutex<Vec<Value>>>,
    tokio::task::JoinHandle<()>,
    std::path::PathBuf,
) {
    let db = fixture().await;
    let path = std::env::temp_dir().join(format!("cal-{}.sock", &security::id()[..8]));
    let unix = UnixListener::bind(&path).unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let worker_calls = calls.clone();
    let worker = Router::new().route("/rpc", post(move |Json(req): Json<Value>| {
        let calls = worker_calls.clone();
        async move {
            assert_eq!(req["alias"], "calendar-fixture");
            assert_eq!(req["credential"], KEY);
            if req["method"] == "tools/list" {
                let declaration = |name, fields: &[&str]| {
                    let properties = fields.iter().map(|key| (String::from(*key), json!({})))
                        .collect::<serde_json::Map<_, _>>();
                    json!({"name":name,"inputSchema":{"type":"object","properties":properties}})
                };
                return Json(json!({"tools":[
                    declaration("get-freebusy",&["account","calendars","timeMin","timeMax"]),
                    declaration("create-event",&["account","calendarId","summary","start","end","attendees","sendUpdates","eventId"]),
                ]}));
            }
            assert_eq!(req["method"], "tools/call");
            let parameters = &req["params"];
            let args = &parameters["arguments"];
            assert_eq!(args["account"], "work");
            calls.lock().unwrap().push(parameters.clone());
            if parameters["name"] == "get-freebusy" {
                assert_eq!(args["calendars"], json!([{"id":"team@example.test"}]));
                return Json(json!({"structuredContent":{
                    "timeMin":START,"timeMax":END,"calendars":{"team@example.test":{"busy":[]}}
                }}));
            }
            assert_eq!(parameters["name"], "create-event");
            Json(if verified {
                json!({"structuredContent":{"event":{
                    "id":args["eventId"],"summary":TITLE,"calendarId":args["calendarId"],"accountId":args["account"],
                    "start":{"dateTime":"2027-01-08T14:00:00Z"},"end":{"dateTime":"2027-01-08T14:30:00Z"},
                    "attendees":[{"email":GUEST}]
                }}})
            } else {
                json!({"content":[{"type":"text","text":"The request was sent."}]})
            })
        }
    }));
    let server = tokio::spawn(async move { axum::serve(unix, worker).await.unwrap() });
    let mut app = app_with_model(&db, Arc::new(CalendarChief));
    app.mcp.stdio_fixture = Some(path.to_string_lossy().into_owned());
    sqlx::query(
        "INSERT INTO connectors(id,owner,name,endpoint,auth,transport)
        VALUES('cal','1','Team calendar','stdio://calendar-fixture','pending','stdio')",
    )
    .execute(&db)
    .await
    .unwrap();
    app.mcp
        .save_api("1", "cal", KEY, "Authorization", "Bearer")
        .await
        .unwrap();
    (app, calls, server, path)
}

async fn draft(app: &App) -> String {
    store::ingest_telegram(
        &app.db,
        "1",
        1,
        "1",
        "Book a calendar meeting for our quarterly review and invite Casey.",
        1,
    )
    .await
    .unwrap();
    let task = store::claim(&app.db).await.unwrap().unwrap();
    runtime::work(app, &task).await.unwrap();
    pending_id(&app.db, &task, "approve").await
}

#[tokio::test]
async fn calendar_checks_availability_reviews_exact_event_and_creates_once() {
    let (app, calls, worker, path) = setup(true).await;
    let app = Arc::new(app);
    let user = signed_mini_init(&app, 1);
    let other = signed_mini_init(&app, 2);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/api/mini", listener.local_addr().unwrap());
    let router = web::router(app.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = reqwest::Client::new();
    let api = |init: &str, action: &str, data: Value| {
        client
            .post(&endpoint)
            .header("origin", &app.origin)
            .json(&json!({"init":init,"action":action,"data":data}))
            .send()
    };
    let selection = json!({"id":"cal","account":"work","calendar":"team@example.test"});
    assert_eq!(
        api(&other, "set-calendar", selection.clone())
            .await
            .unwrap()
            .status(),
        400
    );
    api(&user, "set-calendar", selection)
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let id = draft(&app).await;
    assert_eq!(calls.lock().unwrap().len(), 1);
    let state: Value = api(&user, "status", json!({}))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let request = state["requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|req| req["id"] == id)
        .unwrap();
    let preview = request["content"]["preview"].as_str().unwrap();
    for expected in [
        TITLE,
        START,
        END,
        GUEST,
        "Casey",
        "team@example.test",
        "send invitations",
    ] {
        assert!(preview.contains(expected), "{expected}");
    }
    let pending: String = sqlx::query_scalar("SELECT payload FROM pending WHERE id=?")
        .bind(&id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    for private in [KEY, GUEST, TITLE] {
        assert!(!pending.contains(private));
    }
    assert_eq!(
        api(&other, "approve", json!({"id":id}))
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(calls.lock().unwrap().len(), 1);
    api(&user, "approve", json!({"id":id}))
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let task = store::claim(&app.db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    let write = {
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        calls[1].clone()
    };
    assert_eq!(write["arguments"]["eventId"], id.replace('-', ""));
    assert_eq!(write["arguments"]["sendUpdates"], "all");
    assert_eq!(
        write["arguments"]["attendees"],
        json!([{"email":GUEST,"displayName":"Casey"}])
    );
    assert_eq!(
        api(&user, "approve", json!({"id":id}))
            .await
            .unwrap()
            .status(),
        400
    );
    let messages: Vec<String> = sqlx::query_scalar("SELECT body FROM outbox WHERE task=?")
        .bind(&task.id)
        .fetch_all(&app.db)
        .await
        .unwrap();
    assert!(
        messages
            .iter()
            .any(|msg| msg == "Calendar confirmed the event was added.")
    );
    assert!(
        store::list_mcp_approvals(&app.db, "1")
            .await
            .unwrap()
            .is_empty()
    );
    server.abort();
    worker.abort();
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn calendar_treats_unverified_results_as_uncertain_and_never_resends() {
    let (app, calls, worker, path) = setup(false).await;
    runtime::calendar::configure(&app, "1", "cal", "work", "team@example.test")
        .await
        .unwrap();
    let id = draft(&app).await;
    store::respond(&app.db, "1", &id, true).await.unwrap();
    let task = store::claim(&app.db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    assert_eq!(task_for(&app.db, &task.id).await.state, "uncertain");
    assert!(store::respond(&app.db, "1", &id, true).await.is_err());
    assert!(store::claim(&app.db).await.unwrap().is_none());
    assert_eq!(calls.lock().unwrap().len(), 2);
    worker.abort();
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn calendar_reselection_invalidates_the_pending_event_even_with_identical_values() {
    let (app, calls, worker, path) = setup(true).await;
    runtime::calendar::configure(&app, "1", "cal", "work", "team@example.test")
        .await
        .unwrap();
    let id = draft(&app).await;
    store::respond(&app.db, "1", &id, true).await.unwrap();
    runtime::calendar::configure(&app, "1", "cal", "work", "team@example.test")
        .await
        .unwrap();
    let task = store::claim(&app.db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    assert_eq!(calls.lock().unwrap().len(), 1);
    worker.abort();
    let _ = std::fs::remove_file(path);
}
