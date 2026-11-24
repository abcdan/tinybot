use super::{WidgetChief, app_with_model, fixture, message, signed_mini_init};
use crate::{audit, runtime, security::now, store, web};
use serde_json::{Value, json};
use std::sync::Arc;

#[tokio::test]
async fn each_incoming_message_keeps_its_own_audit_timeline_for_seven_days() {
    let db = fixture().await;
    let app = app_with_model(&db, Arc::new(WidgetChief));
    let before = now();
    message(&db, 1, "1", "Can you make a widget that says hello?").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    let card = store::claim_outbox(&db).await.unwrap().unwrap();
    assert!(card.widget.is_some());

    let recent = audit::sessions(&db, "1", None).await.unwrap();
    let sessions = recent["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    let first = &sessions[0];
    let card_run = first["id"].as_i64().unwrap();
    assert_eq!(first["task"], task.id);
    assert_eq!(first["status"], "completed");
    let started = first["started_at"].as_i64().unwrap();
    assert!(started >= before);
    assert!(first["finished_at"].as_i64().unwrap() >= started);
    assert!(
        first["message"]
            .as_str()
            .unwrap()
            .starts_with("Can you make a widget")
    );
    let trace = audit::session(&db, "1", card_run, 0).await.unwrap();
    let entries = trace["entries"].as_array().unwrap();
    assert_eq!(first["entry_count"], entries.len());
    assert!(
        entries
            .windows(2)
            .all(|pair| { pair[0]["id"].as_i64().unwrap() < pair[1]["id"].as_i64().unwrap() })
    );
    let start = entries
        .iter()
        .position(|entry| entry["kind"] == "model" && entry["status"] == "started")
        .unwrap();
    let complete = entries
        .iter()
        .position(|entry| entry["kind"] == "model" && entry["status"] == "completed")
        .unwrap();
    let card_call = entries
        .iter()
        .position(|entry| {
            entry["kind"] == "tool" && entry["name"] == "send_card" && entry["status"] == "started"
        })
        .unwrap();
    assert!(start < complete && complete < card_call);
    assert!(entries[card_call]["detail"].is_object());
    assert!(!entries[card_call]["detail"].to_string().contains("Hello!"));

    store::ingest_telegram(&db, "1", 2, "1", "React with a thumbs-up.", 52)
        .await
        .unwrap();
    let followup = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(followup.id, task.id);
    runtime::work(&app, &followup).await.unwrap();
    let recent = audit::sessions(&db, "1", None).await.unwrap();
    let sessions = recent["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0]["message"], "React with a thumbs-up.");
    let reaction_run = sessions[0]["id"].as_i64().unwrap();
    assert_ne!(reaction_run, card_run);
    assert_eq!(sessions[1]["id"], card_run);
    assert_eq!(sessions[1]["status"], "completed");
    let trace = audit::session(&db, "1", reaction_run, 0).await.unwrap();
    assert!(trace["entries"].as_array().unwrap().iter().any(|entry| {
        entry["kind"] == "tool"
            && entry["name"] == "react_to_message"
            && entry["status"] == "completed"
            && entry["detail"]["queued"] == true
    }));
    assert_eq!(
        audit::sessions(&db, "2", None).await.unwrap()["sessions"],
        json!([])
    );
    assert!(audit::session(&db, "2", reaction_run, 0).await.is_err());

    sqlx::query("UPDATE audit_sessions SET started_at=? WHERE id=?")
        .bind(now() - 7 * 24 * 60 * 60 - 1)
        .bind(card_run)
        .execute(&db)
        .await
        .unwrap();
    assert!(audit::session(&db, "1", card_run, 0).await.is_err());
    store::expire(&db).await.unwrap();
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM audit_sessions WHERE id=?),
                (SELECT count(*) FROM audit_entries WHERE session=?)",
    )
    .bind(card_run)
    .bind(card_run)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(counts, (0, 0));
    assert!(audit::session(&db, "1", reaction_run, 0).await.is_ok());
}

#[tokio::test]
async fn queued_audit_previews_hide_credentials_and_record_supersession() {
    let db = fixture().await;
    message(&db, 1, "1", "My API key is sk-or-auditfixture").await;
    let recent = audit::sessions(&db, "1", None).await.unwrap();
    let id = recent["sessions"][0]["id"].as_i64().unwrap();
    assert_eq!(recent["sessions"][0]["status"], "queued");
    assert!(!recent.to_string().contains("sk-or-auditfixture"));

    message(&db, 2, "1", "Give me the menu").await;
    let recent = audit::sessions(&db, "1", None).await.unwrap();
    assert_eq!(recent["sessions"][0]["message"], "Give me the menu");
    assert_eq!(recent["sessions"][1]["id"], id);
    assert_eq!(recent["sessions"][1]["status"], "superseded");
}

#[tokio::test]
async fn signed_audit_usage_is_limited_to_the_message_owner_and_retention_window() {
    let db = fixture().await;
    let app = Arc::new(app_with_model(&db, Arc::new(WidgetChief)));
    for (message_id, user, label) in [
        (1, "1", "Expired lookup"),
        (2, "1", "Give me the menu"),
        (3, "2", "Another user's lookup"),
    ] {
        message(&db, message_id, user, label).await;
        sqlx::query("INSERT INTO audit_entries(session,attempt,at,kind,name,status,detail)
            SELECT id,1,?,'model_call','agent_turn','completed',? FROM audit_sessions WHERE message=?")
            .bind(now())
            .bind(json!({"provider":"Test Provider","model":"test/model","usage":{
                "input_tokens":80,"output_tokens":20,"total_tokens":100,"cost_usd":0.015625
            }}).to_string())
            .bind(label)
            .execute(&db)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE audit_sessions SET started_at=? WHERE message='Expired lookup'")
        .bind(now() - 7 * 24 * 60 * 60 - 1)
        .execute(&db)
        .await
        .unwrap();
    let owner = signed_mini_init(&app, 1);
    let other = signed_mini_init(&app, 2);
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
    let rows: Value = post(&owner, "audit-sessions", json!({}))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = rows["sessions"][0]["id"].as_i64().unwrap();
    assert_eq!(rows["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(rows["usage"]["total"]["calls"], 1);
    assert_eq!(rows["usage"]["total"]["total_tokens"], 100);
    assert_eq!(rows["usage"]["providers"][0]["provider"], "Test Provider");
    assert_eq!(rows["usage"]["models"][0]["model"], "test/model");
    assert_eq!(rows["usage"]["models"][0]["cost_usd"], 0.015625);
    let run: Value = post(&owner, "audit-session", json!({"id":id}))
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(run["usage"], rows["usage"]);
    for (user, status) in [(other.as_str(), 404), ("invalid", 403)] {
        for action in ["audit-session", "audit-export"] {
            assert_eq!(
                post(user, action, json!({"id":id})).await.unwrap().status(),
                status
            );
        }
    }
    let expired: i64 =
        sqlx::query_scalar("SELECT id FROM audit_sessions WHERE message='Expired lookup'")
            .fetch_one(&db)
            .await
            .unwrap();
    for unavailable in [expired, expired + 1000] {
        assert_eq!(
            post(&owner, "audit-export", json!({"id":unavailable}))
                .await
                .unwrap()
                .status(),
            404
        );
    }
    server.abort();
}

#[tokio::test]
async fn signed_audit_export_includes_the_entire_redacted_timeline_and_usage() {
    let db = fixture().await;
    let app = Arc::new(app_with_model(&db, Arc::new(WidgetChief)));
    let secret = "sk-or-audit-export-fixture";
    message(&db, 1, "1", &format!("My API key is {secret}")).await;
    let task = store::claim(&db).await.unwrap().unwrap();
    let trace = audit::Trace::start(&db, &task).await.unwrap();
    audit::scope(trace, async {
        for _ in 0..101 {
            audit::record("model", "agent_turn", "started", Value::Null).await;
        }
        audit::tool_started(
            "call_mcp",
            &json!({"tool":"read","arguments":{"value":secret}}),
        )
        .await;
        audit::provider_call("test", "test/model", async |call| {
            call.openrouter(
                &json!({"provider":"Provider Fixture","model":"test/model","usage":{
                    "prompt_tokens":8,"completion_tokens":2,"total_tokens":10,"cost":0.125
                }}),
            );
            Ok(())
        })
        .await
        .unwrap();
    })
    .await;
    sqlx::query("INSERT INTO agents(id,owner,name,instructions,tools) VALUES('shared','2','Research','','[]')")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO grants(agent,member) VALUES('shared','1')")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET agent='shared' WHERE id=?")
        .bind(&task.id)
        .execute(&db)
        .await
        .unwrap();

    let id = audit::sessions(&db, "1", None).await.unwrap()["sessions"][0]["id"]
        .as_i64()
        .unwrap();
    let owner = signed_mini_init(&app, 1);
    let origin = app.origin.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/api/mini", listener.local_addr().unwrap());
    let server =
        tokio::spawn(async move { axum::serve(listener, web::router(app)).await.unwrap() });
    let client = reqwest::Client::new();
    let export = || {
        client
            .post(&endpoint)
            .header("origin", &origin)
            .json(&json!({"init":owner,"action":"audit-export","data":{"id":id}}))
            .send()
    };

    let response = export().await.unwrap().error_for_status().unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let exported: Value = response.json().await.unwrap();
    let first = audit::session(&db, "1", id, 0).await.unwrap();
    let cursor = first["next_after"].as_i64().unwrap();
    let second = audit::session(&db, "1", id, cursor).await.unwrap();
    let mut entries = first["entries"].as_array().unwrap().clone();
    entries.extend(second["entries"].as_array().unwrap().iter().cloned());
    assert_eq!(entries.len(), 103);
    assert_eq!(exported["version"], 1);
    assert_eq!(exported["session"], first["session"]);
    assert_eq!(exported["session"]["message"], "[Sensitive text hidden]");
    assert_eq!(exported["entries"], json!(entries));
    assert_eq!(exported["usage"], first["usage"]);
    assert_eq!(
        exported["usage"]["providers"][0]["provider"],
        "Provider Fixture"
    );
    assert_eq!(exported["usage"]["models"][0]["total_tokens"], 10);
    assert_eq!(exported["usage"]["models"][0]["cost_usd"], 0.125);
    assert!(!exported.to_string().contains(secret));

    sqlx::query("DELETE FROM grants WHERE agent='shared' AND member='1'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(export().await.unwrap().status(), 404);
    server.abort();
}
