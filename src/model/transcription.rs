use super::Models;
use crate::{audit::ModelCall, net};
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::time::Instant;

const ENDPOINT: &str = "https://openrouter.ai/api/v1/audio/transcriptions";
const MODEL: &str = "openai/whisper-large-v3";
pub(crate) const MAX_VOICE_BYTES: usize = 10 * 1024 * 1024;

pub(crate) struct VoiceTranscriber {
    key: String,
}

pub(crate) struct TranscribedVoice {
    pub(crate) text: String,
    pub(crate) usage: ModelCall,
    pub(crate) duration_ms: u64,
}

impl Models {
    pub(crate) async fn voice_transcriber(&self, user: &str) -> Result<Option<VoiceTranscriber>> {
        // Only the user's saved connection can authorize sending and billing this audio.
        Ok(self
            .user_openrouter_key(user)
            .await?
            .map(|key| VoiceTranscriber { key: key.plaintext }))
    }
}

impl VoiceTranscriber {
    pub(crate) async fn transcribe(&self, audio: &[u8]) -> Result<TranscribedVoice> {
        let endpoint = net::https(ENDPOINT)?;
        let request = net::request(reqwest::Method::POST, &endpoint).await?;
        self.request_transcript(request, audio).await
    }

    async fn request_transcript(
        &self,
        request: reqwest::RequestBuilder,
        audio: &[u8],
    ) -> Result<TranscribedVoice> {
        let started = Instant::now();
        let response = request
            .bearer_auth(&self.key)
            .json(&voice_payload(audio)?)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("voice transcription is unavailable"))?;
        ensure!(
            response.status().is_success(),
            "voice transcription was rejected"
        );
        let body = net::body(response, 128 * 1024)
            .await
            .context("invalid voice transcription response")?;
        let reply: Value =
            serde_json::from_slice(&body).context("invalid voice transcription response")?;
        let text = reply["text"]
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .context("voice transcription was empty")?;
        let mut usage = ModelCall::new("openrouter", MODEL);
        usage.openrouter(&reply);
        Ok(TranscribedVoice {
            text: text.to_owned(),
            usage,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }
}

fn voice_payload(audio: &[u8]) -> Result<Value> {
    ensure!(
        audio.len() <= MAX_VOICE_BYTES && audio.starts_with(b"OggS"),
        "invalid Ogg voice note"
    );
    Ok(json!({
        "model":MODEL,
        "input_audio":{"format":"ogg","data":STANDARD.encode(audio)}
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, http::HeaderMap, routing::post};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn voice_transcription_uses_the_saved_key_and_ogg_request() {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&db).await.unwrap();
        crate::store::seed(&db, "42").await.unwrap();

        let vault = crate::security::Vault::new(&"1a".repeat(32)).unwrap();
        let models = Models::new(
            db.clone(),
            vault.clone(),
            "codex".into(),
            None,
            Some("operator".into()),
        );
        assert!(models.voice_transcriber("42").await.unwrap().is_none());

        let credential = vault.seal("openrouter:42", b"synthetic-user-key").unwrap();
        sqlx::query("UPDATE users SET provider='codex', openrouter_key=? WHERE id='42'")
            .bind(credential)
            .execute(&db)
            .await
            .unwrap();
        let transcriber = models.voice_transcriber("42").await.unwrap().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://{}/api/v1/audio/transcriptions",
            listener.local_addr().unwrap()
        );
        let fixture = Router::new().route(
            "/api/v1/audio/transcriptions",
            post(|headers: HeaderMap, Json(value): Json<Value>| async move {
                assert_eq!(headers["authorization"], "Bearer synthetic-user-key");
                assert_eq!(
                    value,
                    json!({
                        "model": "openai/whisper-large-v3",
                        "input_audio": {"data": STANDARD.encode(b"OggStest"), "format": "ogg"}
                    })
                );
                Json(json!({
                    "text": " Hallo Chief. ",
                    "usage": {"seconds": 1, "input_tokens": 11, "output_tokens": 4, "total_tokens": 15, "cost": 0.000008}
                }))
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, fixture).await.unwrap() });
        let request = reqwest::Client::new().post(endpoint);
        let reply = transcriber
            .request_transcript(request, b"OggStest")
            .await
            .unwrap();
        assert_eq!(reply.text, "Hallo Chief.");
        assert_eq!(
            serde_json::to_value(reply.usage).unwrap(),
            json!({
                "provider": "openrouter",
                "model": MODEL,
                "usage": {
                    "input_tokens": 11,
                    "output_tokens": 4,
                    "total_tokens": 15,
                    "cost_usd": 0.000008,
                }
            })
        );
        server.abort();
    }

    #[test]
    fn voice_transcription_rejects_invalid_or_large_audio() {
        assert!(voice_payload(b"not audio").is_err());
        let mut too_large = vec![0; MAX_VOICE_BYTES + 1];
        too_large[..4].copy_from_slice(b"OggS");
        assert!(voice_payload(&too_large).is_err());
    }
}
