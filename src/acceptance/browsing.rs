use super::{app_with_model, fixture, message};
use crate::{model::ModelProvider, runtime, security, store};
use anyhow::{Result, ensure};
use async_trait::async_trait;
use axum::{Json, Router, http::StatusCode, routing::post};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::net::UnixListener;

struct BrowserChief;

#[async_trait]
impl ModelProvider for BrowserChief {
    async fn complete(&self, system: &str, input: &str) -> Result<String> {
        ensure!(
            system.starts_with("You are TinyBot."),
            "the optional reviewer is unavailable"
        );
        let context: Value = serde_json::from_str(input)?;
        let history = context["history"].as_array().unwrap();
        let last = history.last().unwrap();
        let text = last["text"].as_str().unwrap();
        let step = if last["role"] == "user" {
            assert_eq!(text, "Open example.org/docs and tell me what it says.");
            assert!(
                context["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["name"] == "browse")
            );
            json!({"kind":"tool","name":"browse","args":{"url":"example.org/docs"}})
        } else {
            assert_eq!(last["role"], "tool");
            let reply = if let Ok(result) = serde_json::from_str::<Value>(text) {
                assert_eq!(result["url"], "https://example.org/docs");
                assert_eq!(result["text"], "Fixture documentation.");
                "The documentation says: Fixture documentation."
            } else {
                assert!(text.contains("Cloudflare requires verification"));
                "The website requires verification."
            };
            json!({"kind":"reply","text":reply})
        };
        Ok(step.to_string())
    }
}

#[tokio::test]
async fn chat_browse_uses_the_isolated_browser_without_requiring_a_second_model() {
    for (status, result, expected_reply) in [
        (
            StatusCode::OK,
            json!({"url":"https://example.org/docs","text":"Fixture documentation."}),
            "The documentation says: Fixture documentation.",
        ),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({"error":"Cloudflare requires verification before this website can be read."}),
            "The website requires verification.",
        ),
    ] {
        let db = fixture().await;
        let socket = std::env::temp_dir().join(format!("tinybot-{}.sock", &security::id()[..12]));
        let listener = UnixListener::bind(&socket).unwrap();
        let router = Router::new().route(
            "/browse",
            post(move |Json(body): Json<Value>| async move {
                assert_eq!(body, json!({"url":"https://example.org/docs"}));
                (status, Json(result))
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut app = app_with_model(&db, Arc::new(BrowserChief));
        app.workers.browser = socket.to_string_lossy().into_owned();
        message(
            &db,
            1,
            "1",
            "Open example.org/docs and tell me what it says.",
        )
        .await;
        let task = store::claim(&db).await.unwrap().unwrap();
        let outcome = runtime::work(&app, &task).await;
        server.abort();
        std::fs::remove_file(socket).unwrap();
        outcome.unwrap();

        let reply = store::claim_outbox(&db).await.unwrap().unwrap();
        assert_eq!(reply.body, expected_reply);
        assert_eq!(reply.task.as_deref(), Some(task.id.as_str()));
        let pending: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pending WHERE task=? AND state='waiting'")
                .bind(&task.id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(pending, 0);
    }
}
