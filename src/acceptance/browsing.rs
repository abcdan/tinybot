use super::{app_with_model, fixture, message};
use crate::{
    model::{ModelProvider, ProviderFailure, TextDelta, ToolDefinition},
    runtime::{self, Intent, Outcome, Risk, Tool, ToolContext},
    security, store,
};
use anyhow::{Result, ensure};
use async_trait::async_trait;
use axum::{Json, Router, http::StatusCode, routing::post};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
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
            let observation: Value = serde_json::from_str(text)?;
            assert_eq!(observation["tool"], "browse");
            assert_eq!(observation["target"], "https://example.org/docs");
            let reply = if observation["status"] == "ok" {
                let result = &observation["result"];
                assert_eq!(result["url"], "https://example.org/docs");
                assert_eq!(result["text"], "Fixture documentation.");
                "The documentation says: Fixture documentation."
            } else {
                assert_eq!(observation["status"], "failed");
                assert!(
                    observation["error"]
                        .as_str()
                        .unwrap()
                        .contains("Cloudflare requires verification")
                );
                "The website requires verification."
            };
            json!({"kind":"reply","text":reply})
        };
        Ok(step.to_string())
    }
}

struct RepeatingBrowser {
    calls: AtomicUsize,
    blocked: bool,
}

#[async_trait]
impl ModelProvider for RepeatingBrowser {
    async fn complete(&self, system: &str, input: &str) -> Result<String> {
        assert!(system.contains("Give a final answer"));
        let input: Value = serde_json::from_str(input)?;
        let observations: Vec<Value> = input["history"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["role"] == "tool")
            .filter_map(|event| serde_json::from_str(event["text"].as_str()?).ok())
            .collect();
        assert_eq!(observations.len(), 1);
        let observation = &observations[0];
        assert_eq!(observation["target"], "https://example.org/");
        if self.blocked {
            assert_eq!(observation["status"], "failed");
            assert!(
                observation["error"]
                    .as_str()
                    .unwrap()
                    .contains("verification")
            );
            return Ok(json!({"text":"De website vraagt om verificatie, dus ik kon de pagina niet lezen."}).to_string());
        }
        assert_eq!(observation["result"]["text"], "Een digitale studio.");
        Ok(json!({"text":"Het is een digitale studio. https://example.org/"}).to_string())
    }

    async fn complete_turn(
        &self,
        _: &str,
        _: &str,
        tools: &[ToolDefinition],
        _: Option<&mut TextDelta<'_>>,
    ) -> Result<String> {
        assert!(tools.iter().any(|tool| tool.name == "browse"));
        let call = self.calls.fetch_add(1, Ordering::Relaxed);
        let url = ["example.org", "https://EXAMPLE.org/", "http://example.org/"][call];
        Ok(json!({"kind":"tool","name":"browse","args":{"url":url}}).to_string())
    }
}

#[tokio::test]
async fn a_repeated_lookup_finishes_from_the_first_browser_result() {
    for blocked in [false, true] {
        let db = fixture().await;
        let socket = std::env::temp_dir().join(format!("tinybot-{}.sock", &security::id()[..12]));
        let listener = UnixListener::bind(&socket).unwrap();
        let reads = Arc::new(AtomicUsize::new(0));
        let router = Router::new().route("/browse", post({
            let reads = reads.clone();
            move |Json(body): Json<Value>| {
                let reads = reads.clone();
                async move {
                    assert_eq!(body["url"], "https://example.org/");
                    reads.fetch_add(1, Ordering::Relaxed);
                    if blocked {
                        (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"error":"Cloudflare requires verification."})))
                    } else {
                        (StatusCode::OK, Json(json!({"url":"https://example.org/","text":"Een digitale studio."})))
                    }
                }
            }
        }));
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let model = Arc::new(RepeatingBrowser {
            calls: AtomicUsize::new(0),
            blocked,
        });
        let mut app = app_with_model(&db, model.clone());
        app.workers.browser = socket.to_string_lossy().into_owned();
        message(
            &db,
            1,
            "1",
            "Kan je via je browser zoeken naar example.org?",
        )
        .await;
        let task = store::claim(&db).await.unwrap().unwrap();
        let outcome = runtime::work(&app, &task).await;
        server.abort();
        std::fs::remove_file(socket).unwrap();
        outcome.unwrap();

        assert_eq!(reads.load(Ordering::Relaxed), 1);
        assert_eq!(model.calls.load(Ordering::Relaxed), 3);
        assert_eq!(
            store::claim_outbox(&db).await.unwrap().unwrap().body,
            if blocked {
                "De website vraagt om verificatie, dus ik kon de pagina niet lezen."
            } else {
                "Het is een digitale studio. https://example.org/"
            }
        );
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

struct UnrelatedSearch;

#[async_trait]
impl Tool for UnrelatedSearch {
    fn name(&self) -> &'static str {
        "search_web"
    }

    fn describe(&self) -> &'static str {
        "Return unrelated indexed results"
    }

    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{"query":{"type":"string"}}})
    }

    async fn inspect(&self, _: &ToolContext<'_>, _: &Value) -> Result<Intent> {
        Ok(Intent {
            risk: Risk::PublicRead,
            detail: "search the public web".into(),
            destination: "Public web search".into(),
        })
    }

    async fn execute(&self, _: &ToolContext<'_>, _: Value) -> Result<Outcome> {
        Ok(Outcome::Continue(json!({
            "results":[{"title":"Solitaire","url":"https://games.example/solitaire"}]
        })))
    }
}

struct ResearchFinal {
    after_handoff: bool,
}

#[async_trait]
impl ModelProvider for ResearchFinal {
    async fn complete_turn(
        &self,
        _: &str,
        _: &str,
        _: &[ToolDefinition],
        _: Option<&mut TextDelta<'_>>,
    ) -> Result<String> {
        if self.after_handoff {
            anyhow::bail!("the Chief's tool turn failed");
        }
        Ok(
            json!({"kind":"tool","name":"search_web","args":{"query":"MaxiAxi retourkosten"}})
                .to_string(),
        )
    }

    async fn complete(&self, system: &str, input: &str) -> Result<String> {
        assert!(system.contains("Use only evidence that directly addresses the request"));
        let context: Value = serde_json::from_str(input)?;
        assert_eq!(context["latest_request"], "Kan je even doorzoeken?");
        assert_eq!(
            context["telegram_reply"]["excerpt"],
            "Ik zoek de retourvoorwaarden van MaxiAxi."
        );
        if self.after_handoff {
            assert!(input.contains("retourpagina"));
            return Ok(
                json!({"text":"Op de retourpagina staat dat je eerst contact moet opnemen."})
                    .to_string(),
            );
        }
        assert!(input.contains("Solitaire"));
        Ok("invalid model response".into())
    }
}

#[tokio::test]
async fn chief_finishes_research_without_dumping_unrelated_results() {
    for after_handoff in [false, true] {
        let db = fixture().await;
        let mut app = app_with_model(&db, Arc::new(ResearchFinal { after_handoff }));
        app.tools.retain(|tool| tool.name() != "search_web");
        app.tools.push(Box::new(UnrelatedSearch));
        store::ingest_telegram_reply(
            &db,
            "1",
            42,
            "1",
            "Kan je even doorzoeken?",
            55,
            Some(store::ReplyContext {
                message_id: 54,
                excerpt: Some("Ik zoek de retourvoorwaarden van MaxiAxi.".into()),
                from_bot: true,
            }),
        )
        .await
        .unwrap();
        super::expect_no_immediate_telegram_ack(&db).await;
        let task = store::claim(&db).await.unwrap().unwrap();
        if after_handoff {
            store::append(&db, &task.id, "handoff", "Ik vraag het aan Research.")
                .await
                .unwrap();
            store::append(
                &db,
                &task.id,
                "tool",
                r#"{"helper_reply":"Op de retourpagina staat dat je eerst contact moet opnemen."}"#,
            )
            .await
            .unwrap();
        }
        runtime::work(&app, &task).await.unwrap();
        let final_reply = store::claim_outbox(&db).await.unwrap().unwrap();
        assert_eq!(
            final_reply.body,
            if after_handoff {
                "Op de retourpagina staat dat je eerst contact moet opnemen."
            } else {
                "I couldn’t verify the requested information."
            }
        );
    }
}

const SHARED_PAGE: &str = "https://example.org/shared-cart";
const CART_ITEM: &str = "Artikel aan het eind van de pagina: groene thee.";
const CART_REPLY: &str = "Ik zie groene thee in de gedeelde winkelwagen.\n\nBronnen:\n[Winkelwagen](https://example.org/shared-cart)";

struct SharedPageRecovery(AtomicUsize);

#[async_trait]
impl ModelProvider for SharedPageRecovery {
    async fn complete(&self, instructions: &str, _: &str) -> Result<String> {
        assert!(instructions.starts_with("Summarize only durable task context"));
        Ok(json!({"summary":"The user asked to inspect the shared cart."}).to_string())
    }

    async fn complete_turn(
        &self,
        instructions: &str,
        input: &str,
        tools: &[ToolDefinition],
        _: Option<&mut TextDelta<'_>>,
    ) -> Result<String> {
        match self.0.fetch_add(1, Ordering::Relaxed) {
            0 => Ok(json!({"kind":"tool","name":"search_web","args":{"query":format!("search for {SHARED_PAGE}")}}).to_string()),
            1 => {
                assert!(!tools.is_empty());
                let context: Value = serde_json::from_str(input)?;
                let history = context["history"].as_array().unwrap();
                assert_eq!(history.last().unwrap()["role"], "tool");
                assert_eq!(history.last().unwrap()["text"].as_str().unwrap().len(), 12_000);
                assert!(!input.contains(CART_ITEM));
                Err(ProviderFailure::OutputLimit.into())
            }
            2 => {
                assert!(tools.is_empty());
                assert!(instructions.contains("user-facing reply in plain text"));
                let context: Value = serde_json::from_str(input)?;
                assert_eq!(context["opened_page"]["url"], SHARED_PAGE);
                assert_eq!(context["opened_page"]["title"], "Winkelwagen");
                assert!(
                    context["opened_page"]["text"]
                        .as_str()
                        .unwrap()
                        .ends_with(CART_ITEM)
                );
                assert!(!input.contains("Solitaire"));
                Ok(CART_REPLY.into())
            }
            _ => panic!("unexpected extra model call"),
        }
    }
}

#[tokio::test]
async fn a_shared_url_is_opened_and_kept_for_plain_text_limit_recovery() {
    let db = fixture().await;
    let socket = std::env::temp_dir().join(format!("tinybot-{}.sock", &security::id()[..12]));
    let listener = UnixListener::bind(&socket).unwrap();
    let router = Router::new().route(
        "/browse",
        post(|Json(body): Json<Value>| async move {
            assert_eq!(body, json!({"url":SHARED_PAGE}));
            let body = "Inhoud van de gedeelde winkelwagen. ".repeat(115);
            let links: Vec<_> = (0..16)
                .map(|index| json!({"text":"Bijlage ".repeat(10),"url":format!("https://example.org/{index}/{}","a".repeat(470))}))
                .collect();
            Json(json!({"url":SHARED_PAGE,"title":"Winkelwagen","text":format!("{body}{CART_ITEM}"),"links":links}))
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let model = Arc::new(SharedPageRecovery(AtomicUsize::new(0)));
    let mut app = app_with_model(&db, model.clone());
    app.workers.browser = socket.to_string_lossy().into_owned();
    app.tools.retain(|tool| tool.name() != "search_web");
    app.tools.push(Box::new(UnrelatedSearch));
    message(&db, 1, "1", "Bekijk de bestelling.").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    for _ in 0..7 {
        store::append(&db, &task.id, "assistant", &"eerdere inhoud ".repeat(280))
            .await
            .unwrap();
    }
    store::settle(&db, &task, "idle", None).await.unwrap();
    message(
        &db,
        2,
        "1",
        &format!("Dit is de winkelwagen. {SHARED_PAGE}"),
    )
    .await;
    let continued = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(continued.id, task.id);

    let outcome = runtime::work(&app, &continued).await;
    server.abort();
    std::fs::remove_file(socket).unwrap();
    outcome.unwrap();

    assert_eq!(model.0.load(Ordering::Relaxed), 3);
    assert_eq!(
        store::claim_outbox(&db).await.unwrap().unwrap().body,
        CART_REPLY
    );
}
