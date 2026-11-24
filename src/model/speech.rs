use super::Models;
use crate::net;
use anyhow::{Context, Result, ensure};
use serde_json::json;

const ENDPOINT: &str = "https://openrouter.ai/api/v1/audio/speech";
const MODEL: &str = "mistralai/voxtral-mini-tts-2603";

impl Models {
    pub(crate) async fn synthesize_reply(&self, user: &str, text: &str) -> Result<Vec<u8>> {
        let key = self
            .user_openrouter_key(user)
            .await?
            .context("Connect OpenRouter to listen to replies.")?;
        let request = net::request(reqwest::Method::POST, &net::https(ENDPOINT)?).await?;
        request_speech(request, &key.plaintext, text).await
    }
}

async fn request_speech(
    request: reqwest::RequestBuilder,
    key: &str,
    text: &str,
) -> Result<Vec<u8>> {
    ensure!(
        !text.is_empty() && text.len() <= 4_000,
        "invalid speech text"
    );
    let response = request
        .timeout(std::time::Duration::from_secs(20))
        .bearer_auth(key)
        .json(
            &json!({"model":MODEL,"voice":"en_paul_neutral","input":text,"response_format":"mp3"}),
        )
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("speech synthesis is unavailable"))?;
    ensure!(
        response.status().is_success(),
        "speech synthesis was rejected"
    );
    let bytes = net::body(response, 4 * 1024 * 1024).await?;
    ensure!(
        bytes.starts_with(b"ID3")
            || bytes
                .get(..2)
                .is_some_and(|prefix| prefix[0] == 0xff && prefix[1] & 0xe0 == 0xe0),
        "speech synthesis did not return MP3 audio"
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, http::HeaderMap, routing::post};
    use serde_json::Value;

    #[tokio::test]
    async fn speech_requests_mp3_with_the_supplied_user_key() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/audio/speech", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/audio/speech",
            post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                assert_eq!(headers["authorization"], "Bearer fixture-key");
                assert_eq!(body["model"], MODEL);
                assert_eq!(body["voice"], "en_paul_neutral");
                assert_eq!(body["input"], "Read the report.");
                assert_eq!(body["response_format"], "mp3");
                (
                    [(axum::http::header::CONTENT_TYPE, "audio/mpeg")],
                    b"ID3fixture".as_slice(),
                )
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let bytes = request_speech(
            reqwest::Client::new().post(&endpoint),
            "fixture-key",
            "Read the report.",
        )
        .await
        .unwrap();
        assert_eq!(bytes, b"ID3fixture");
        server.abort();
    }
}
