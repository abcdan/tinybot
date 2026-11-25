use super::*;
use axum::{Json, Router, http::header, routing::post};
use base64::{Engine, engine::general_purpose::STANDARD};
use store::outputs;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfixture output";
const MP4: &[u8] = b"\x00\x00\x00\x18ftypisomfixture output";
const JPEG: &[u8] = b"\xff\xd8\xfffixture upload";

struct MediaChief {
    tool: &'static str,
    args: Value,
    follow_up: Option<(&'static str, &'static str)>,
}

#[async_trait]
impl ModelProvider for MediaChief {
    async fn complete(&self, system: &str, _: &str) -> Result<String> {
        assert!(system.starts_with("Review this proposed TinyBot tool action"));
        Ok(r#"{"verdict":"allow","reason":"The user requested this image."}"#.into())
    }

    async fn complete_turn(
        &self,
        _: &str,
        input: &str,
        tools: &[ToolDefinition],
        _: Option<&mut TextDelta<'_>>,
    ) -> Result<String> {
        let context: Value = serde_json::from_str(input)?;
        let last = context["history"].as_array().unwrap().last().unwrap();
        let response = if last["role"] == "user" {
            let tool = tools.iter().find(|tool| tool.name == self.tool).unwrap();
            assert!(
                tool.input_schema["properties"]
                    .get("_image_model")
                    .is_none()
            );
            json!({"kind":"tool","name":self.tool,"args":self.args})
        } else {
            assert_eq!(last["role"], "tool");
            let (cookie_dialog, text) = self.follow_up.expect("the media should finish the turn");
            let observation: Value = serde_json::from_str(last["text"].as_str().unwrap())?;
            assert_eq!(
                observation,
                json!({"queued":true,"media_delivery":self.tool,"cookie_dialog":cookie_dialog})
            );
            json!({"kind":"reply","text":text})
        };
        Ok(response.to_string())
    }
}

async fn expect_media(
    app: &App,
    format: &str,
    bytes: &[u8],
    follow_up: Option<&str>,
    planned: bool,
) {
    let task = store::claim(&app.db).await.unwrap().unwrap();
    if planned {
        store::checkpoints::save_plan(&app.db, &task, &["Capture the page", "Describe the layout"])
            .await
            .unwrap();
    }
    runtime::work(app, &task).await.unwrap();
    let outbound = store::claim_outbox(&app.db).await.unwrap().unwrap();
    assert_eq!(outbound.reply_to_message_id, Some(27));
    let output = outputs::load(&app.db, &app.vault, &outbound)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.kind, format);
    assert_eq!(output.bytes, bytes);
    assert!(output.ephemeral);
    if let Some(text) = follow_up {
        assert_eq!(
            store::claim_outbox(&app.db).await.unwrap().unwrap().body,
            text
        );
    }
    assert!(store::claim_outbox(&app.db).await.unwrap().is_none());
    assert_eq!(task_for(&app.db, &task.id).await.state, "idle");
}

#[cfg(unix)]
#[tokio::test]
async fn chief_sends_screenshot_and_recording_from_the_browser() {
    use tokio::net::UnixListener;

    let socket = std::env::temp_dir().join(format!("tinybot-{}.sock", &security::id()[..12]));
    let listener = UnixListener::bind(&socket).unwrap();
    let router = Router::new()
        .route(
            "/screenshot",
            post(|Json(body): Json<Value>| async move {
                let status = match (body["url"].as_str(), body["dismiss_cookie_dialog"].as_bool()) {
                    (Some("https://example.org/docs"), Some(true)) => "dismissed",
                    (Some("https://example.org/docs"), Some(false))
                    | (Some("https://example.org/blocked"), Some(true)) => "visible",
                    _ => panic!("unexpected screenshot request"),
                };
                (
                    [(header::CONTENT_TYPE.as_str(), "image/png"), ("x-tinybot-cookie-dialog", status)],
                    PNG,
                )
            }),
        )
        .route(
            "/record",
            post(|Json(body): Json<Value>| async move {
                assert_eq!(
                    body,
                    json!({"url":"https://example.org/docs","duration_seconds":2,"scroll":true,"dismiss_cookie_dialog":true})
                );
                (
                    [(header::CONTENT_TYPE.as_str(), "video/mp4"), ("x-tinybot-cookie-dialog", "dismissed")],
                    MP4,
                )
            }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    for (tool, args, format, bytes, follow_up, planned) in [
        (
            "screenshot_page",
            json!({"url":"example.org/docs"}),
            "png",
            PNG,
            None,
            false,
        ),
        (
            "screenshot_page",
            json!({"url":"example.org/docs","dismiss_cookie_dialog":false}),
            "png",
            PNG,
            None,
            false,
        ),
        (
            "screenshot_page",
            json!({"url":"example.org/blocked"}),
            "png",
            PNG,
            Some((
                "visible",
                "This page's cookie dialog still covers the main content.",
            )),
            false,
        ),
        (
            "record_page",
            json!({"url":"https://example.org/docs","duration_seconds":2,"scroll":true,"continue_task":true}),
            "mp4",
            MP4,
            Some(("dismissed", "The page has a table of contents.")),
            false,
        ),
        (
            "screenshot_page",
            json!({"url":"example.org/docs"}),
            "png",
            PNG,
            Some(("dismissed", "The page has a table of contents.")),
            true,
        ),
    ] {
        let db = fixture().await;
        let mut app = app_with_model(
            &db,
            Arc::new(MediaChief {
                tool,
                args,
                follow_up,
            }),
        );
        app.workers.browser = socket.to_string_lossy().into_owned();
        store::ingest_telegram(&db, "1", 1, "1", "Capture example.org/docs", 27)
            .await
            .unwrap();
        expect_media(
            &app,
            format,
            bytes,
            follow_up.map(|(_, text)| text),
            planned,
        )
        .await;
    }
    server.abort();
    std::fs::remove_file(socket).unwrap();
}

#[tokio::test]
async fn chief_generates_and_edits_images_using_the_personal_image_model() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/images", listener.local_addr().unwrap());
    let router = Router::new().route(
        "/images",
        post(|Json(body): Json<Value>| async move {
            if body["prompt"] == "Paint a mountain." {
                assert_eq!(body["model"], "google/gemini-3.1-flash-image");
                assert!(body.get("input_references").is_none());
            } else {
                assert_eq!(body["prompt"], "Paint the mountain blue.");
                assert_eq!(body["model"], "google/gemini-3-pro-image");
                assert_eq!(
                    body["input_references"][0]["image_url"]["url"],
                    format!("data:image/jpeg;base64,{}", STANDARD.encode(JPEG))
                );
            }
            Json(json!({"data":[{"b64_json":STANDARD.encode(PNG)}]}))
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    for (tool, prompt) in [
        ("generate_image", "Paint a mountain."),
        ("edit_image", "Paint the mountain blue."),
    ] {
        let db = fixture().await;
        let mut app = app_with_model(
            &db,
            Arc::new(MediaChief {
                tool,
                args: json!({"prompt":prompt}),
                follow_up: None,
            }),
        );
        app.models = app.models.with_image_fixture(endpoint.clone());
        sqlx::query("UPDATE users SET openrouter_key=? WHERE id='1'")
            .bind(app.vault.seal("openrouter:1", b"fixture-key").unwrap())
            .execute(&db)
            .await
            .unwrap();
        if tool == "edit_image" {
            crate::settings::update_personal(
                &db,
                "1",
                crate::settings::Key::ImageModel,
                "google/gemini-3-pro-image",
            )
            .await
            .unwrap();
            let source = store::attachments::seal(
                &app.vault,
                "1",
                27,
                "mountain.jpg".into(),
                "image/jpeg",
                JPEG,
            )
            .unwrap();
            store::ingest_telegram_attachment(&db, "1", 1, "1", prompt, 27, None, source)
                .await
                .unwrap();
        } else {
            store::ingest_telegram(&db, "1", 1, "1", prompt, 27)
                .await
                .unwrap();
        }
        expect_media(&app, "png", PNG, None, false).await;
    }
    server.abort();
}
