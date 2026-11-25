use super::{Models, ProviderFailure, failure, media::data_url};
use crate::{audit, net};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Serialize;
use serde_json::{Value, json};
use std::time::Duration;

const ENDPOINT: &str = "https://openrouter.ai/api/v1/images";
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_ENCODED_BYTES: usize = MAX_IMAGE_BYTES.div_ceil(3) * 4;
pub(crate) const MAX_IMAGE_PROMPT_BYTES: usize = 4_000;
pub(super) const DEFAULT_IMAGE_MODEL: &str = "google/gemini-3.1-flash-image";

#[derive(Serialize)]
pub(crate) struct ImageModel {
    pub(crate) id: &'static str,
    pub(crate) name: &'static str,
}

pub(crate) const IMAGE_MODELS: &[ImageModel] = &[
    ImageModel {
        id: DEFAULT_IMAGE_MODEL,
        name: "Gemini Nano Banana 2 (default)",
    },
    ImageModel {
        id: "google/gemini-3.1-flash-lite-image",
        name: "Gemini Nano Banana 2 Lite",
    },
    ImageModel {
        id: "google/gemini-3-pro-image",
        name: "Gemini Nano Banana Pro",
    },
    ImageModel {
        id: "openai/gpt-image-2.6-flare",
        name: "GPT Image 2.6 Flare",
    },
];

pub(crate) fn resolve_image_model(saved: Option<&str>) -> &'static str {
    IMAGE_MODELS
        .iter()
        .find(|entry| saved == Some(entry.id))
        .map_or(DEFAULT_IMAGE_MODEL, |entry| entry.id)
}

pub(crate) struct GeneratedImage {
    pub(crate) mime: &'static str,
    pub(crate) bytes: Vec<u8>,
}

impl Models {
    pub(crate) async fn render_image(
        &self,
        user: &str,
        expected_model: &str,
        prompt: &str,
        source: Option<(&str, &[u8])>,
    ) -> Result<GeneratedImage> {
        let model = self.image_model(user).await?;
        ensure!(
            model == expected_model,
            "The image model changed after approval."
        );
        let body = image_request(model, prompt, source)?;
        let key = self
            .user_openrouter_key(user)
            .await?
            .context(ProviderFailure::OpenRouterKey)?;
        audit::provider_call("openrouter", model, async |call| {
            let request = self.image_request_builder().await?;
            let result = request_image(request, &key.plaintext, &body).await?;
            call.openrouter(&result);
            decode_image(&result)
        })
        .await
    }

    async fn image_request_builder(&self) -> Result<reqwest::RequestBuilder> {
        #[cfg(test)]
        if let Some(endpoint) = &self.image_fixture {
            return Ok(reqwest::Client::new().post(endpoint));
        }
        net::request(reqwest::Method::POST, &net::https(ENDPOINT)?)
            .await
            .map_err(|_| ProviderFailure::Temporary.into())
    }

    #[cfg(test)]
    pub(crate) fn with_image_fixture(mut self, endpoint: String) -> Self {
        self.image_fixture = Some(endpoint);
        self
    }
}

fn image_request(model: &str, prompt: &str, source: Option<(&str, &[u8])>) -> Result<Value> {
    let prompt = prompt.trim();
    ensure!(
        !prompt.is_empty() && prompt.len() <= MAX_IMAGE_PROMPT_BYTES,
        "Provide an image prompt up to 4,000 bytes."
    );
    let mut body = json!({"model":model,"prompt":prompt,"n":1,"response_format":"b64_json"});
    if let Some((mime, bytes)) = source {
        ensure!(
            bytes.len() <= MAX_IMAGE_BYTES && image_mime(bytes) == Some(mime),
            "Send a PNG, JPEG or WebP image up to 10 MiB."
        );
        body["input_references"] =
            json!([{"type":"image_url","image_url":{"url":data_url(mime, bytes)}}]);
    }
    Ok(body)
}

async fn request_image(request: reqwest::RequestBuilder, key: &str, body: &Value) -> Result<Value> {
    let response = request
        .timeout(Duration::from_secs(120))
        .bearer_auth(key)
        .json(body)
        .send()
        .await
        .map_err(|error| {
            failure::http_transport_failure(&error).unwrap_or(ProviderFailure::Rejected)
        })?;
    if !response.status().is_success() {
        return Err(match response.status().as_u16() {
            401 => ProviderFailure::OpenRouterKey,
            402 => ProviderFailure::OpenRouterCredits,
            403 => ProviderFailure::AccessDenied,
            404 => ProviderFailure::ModelUnavailable,
            code => failure::transient_status(u64::from(code)).unwrap_or(ProviderFailure::Rejected),
        }
        .into());
    }
    let body = net::body(response, MAX_ENCODED_BYTES + 64 * 1024)
        .await
        .map_err(|_| ProviderFailure::Incomplete)?;
    serde_json::from_slice(&body).map_err(|_| ProviderFailure::Incomplete.into())
}

fn decode_image(result: &Value) -> Result<GeneratedImage> {
    let Some([image]) = result["data"].as_array().map(Vec::as_slice) else {
        return Err(ProviderFailure::Incomplete.into());
    };
    let encoded = image["b64_json"]
        .as_str()
        .filter(|encoded| !encoded.is_empty() && encoded.len() <= MAX_ENCODED_BYTES)
        .context(ProviderFailure::Incomplete)?;
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| ProviderFailure::Incomplete)?;
    let mime = image_mime(&bytes).context(ProviderFailure::Incomplete)?;
    ensure!(
        bytes.len() <= MAX_IMAGE_BYTES
            && (image["media_type"].is_null() || image["media_type"] == mime),
        ProviderFailure::Incomplete
    );
    Ok(GeneratedImage { mime, bytes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::provider_failure,
        security::Vault,
        settings::{self, Key},
        store,
    };
    use axum::{Json, Router, http::HeaderMap, routing::post};
    use sqlx::sqlite::SqlitePoolOptions;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfixture";
    const JPEG: &[u8] = b"\xff\xd8\xfffixture";

    #[tokio::test]
    async fn generation_and_editing_use_each_users_selected_model_and_key() {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&db).await.unwrap();
        for user in ["1", "2"] {
            store::seed(&db, user).await.unwrap();
        }
        let vault = Vault::new(&"ef".repeat(32)).unwrap();
        sqlx::query(
            "UPDATE users SET provider='codex', model='task-model', openrouter_key=? WHERE id='1'",
        )
        .bind(vault.seal("openrouter:1", b"sk-or-fixture").unwrap())
        .execute(&db)
        .await
        .unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/images", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let app = Router::new().route("/images", post(image_fixture));
            axum::serve(listener, app).await.unwrap();
        });
        let models = Models::new(
            db.clone(),
            vault,
            "codex".into(),
            None,
            Some("review-fixture".into()),
        )
        .with_image_fixture(endpoint);
        assert!(models.image_model_available("1").await.unwrap());
        assert!(!models.image_model_available("2").await.unwrap());
        assert_eq!(models.image_model("1").await.unwrap(), DEFAULT_IMAGE_MODEL);
        assert_eq!(models.image_model("2").await.unwrap(), DEFAULT_IMAGE_MODEL);
        let image = models
            .render_image("1", DEFAULT_IMAGE_MODEL, "Draw a moon.", None)
            .await
            .unwrap();
        assert_eq!(image.mime, "image/png");
        assert_eq!(image.bytes, PNG);

        let pro = "google/gemini-3-pro-image";
        settings::update_personal(&db, "1", Key::ImageModel, pro)
            .await
            .unwrap();
        assert!(
            settings::update_personal(&db, "1", Key::ImageModel, "made-up/model")
                .await
                .is_err()
        );
        assert_eq!(models.image_model("1").await.unwrap(), pro);
        assert_eq!(models.image_model("2").await.unwrap(), DEFAULT_IMAGE_MODEL);
        let task_model: String = sqlx::query_scalar("SELECT model FROM users WHERE id='1'")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(task_model, "task-model");

        let stale = models
            .render_image(
                "1",
                DEFAULT_IMAGE_MODEL,
                "Make it blue.",
                Some(("image/jpeg", JPEG)),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(stale.to_string(), "The image model changed after approval.");
        let edited = models
            .render_image("1", pro, "Make it blue.", Some(("image/jpeg", JPEG)))
            .await
            .unwrap();
        assert_eq!(edited.mime, "image/png");
        let missing_key = models
            .render_image("2", DEFAULT_IMAGE_MODEL, "Draw a moon.", None)
            .await
            .err()
            .unwrap();
        assert_eq!(
            provider_failure(&missing_key),
            Some(ProviderFailure::OpenRouterKey)
        );
        server.abort();
    }

    async fn image_fixture(headers: HeaderMap, Json(body): Json<Value>) -> Json<Value> {
        assert_eq!(headers["authorization"], "Bearer sk-or-fixture");
        assert_eq!(body["response_format"], "b64_json");
        assert_eq!(body["n"], 1);
        match body["prompt"].as_str().unwrap() {
            "Draw a moon." => {
                assert_eq!(body["model"], DEFAULT_IMAGE_MODEL);
                assert!(body.get("input_references").is_none());
            }
            "Make it blue." => {
                assert_eq!(body["model"], "google/gemini-3-pro-image");
                assert_eq!(
                    body["input_references"],
                    json!([
                        {"type":"image_url","image_url":{"url":data_url("image/jpeg", JPEG)}}
                    ])
                );
            }
            _ => panic!("unexpected image request"),
        }
        Json(json!({"data":[{"b64_json":STANDARD.encode(PNG),"media_type":"image/png"}]}))
    }

    #[test]
    fn validates_inline_images_and_mime_types() {
        let b64 = STANDARD.encode(JPEG);
        let jpeg = decode_image(&json!({"data":[{"b64_json":b64}]})).unwrap();
        assert_eq!(jpeg.mime, "image/jpeg");
        let mut oversized = JPEG.to_vec();
        oversized.resize(MAX_IMAGE_BYTES + 1, 0);
        for invalid in [
            json!({"url":"https://fixture.invalid/image.jpg"}),
            json!({"b64_json":b64,"media_type":"image/png"}),
            json!({"b64_json":b64,"media_type":7}),
            json!({"b64_json":STANDARD.encode(b"not an image")}),
            json!({"b64_json":STANDARD.encode(oversized)}),
        ] {
            assert!(decode_image(&json!({"data":[invalid]})).is_err());
        }
        assert!(image_request(DEFAULT_IMAGE_MODEL, "Edit.", Some(("image/png", JPEG))).is_err());
        let prompt = "x".repeat(MAX_IMAGE_PROMPT_BYTES + 1);
        assert!(image_request(DEFAULT_IMAGE_MODEL, &prompt, None).is_err());
    }
}

fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}
