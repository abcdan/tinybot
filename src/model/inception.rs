use super::{
    ModelProvider, ProviderFailure,
    failure::{http_transport_failure, transient_status},
};
use crate::{audit, net};
use anyhow::{Result, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{sync::LazyLock, time::Duration};

const CHAT_COMPLETIONS: &str = "https://api.inceptionlabs.ai/v1/chat/completions";
const MODEL: &str = "mercury-3";
const MAX_RESPONSE_BYTES: usize = 16_384;

static CLIENT: LazyLock<Result<reqwest::Client, reqwest::Error>> = LazyLock::new(|| {
    reqwest::Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_millis(1800))
        .pool_max_idle_per_host(4)
        .build()
});

pub(super) struct Mercury3 {
    pub(super) key: String,
}

#[async_trait]
impl ModelProvider for Mercury3 {
    async fn complete(&self, system: &str, input: &str) -> Result<String> {
        audit::provider_call("inception", MODEL, async |call| {
            let response = CLIENT
                .as_ref()
                .map_err(|_| ProviderFailure::Temporary)?
                .post(CHAT_COMPLETIONS)
                .bearer_auth(&self.key)
                .json(&brief_request(system, input))
                .send()
                .await
                .map_err(|error| {
                    http_transport_failure(&error).unwrap_or(ProviderFailure::Rejected)
                })?;
            let status = response.status();
            if !status.is_success() {
                return Err(transient_status(u64::from(status.as_u16()))
                    .unwrap_or(ProviderFailure::Rejected)
                    .into());
            }
            let bytes = net::body(response, MAX_RESPONSE_BYTES).await?;
            let payload =
                serde_json::from_slice(&bytes).map_err(|_| ProviderFailure::Incomplete)?;
            // Inception reports usage using the same chat completion fields.
            call.openrouter(&payload);
            completed_message(&payload)
        })
        .await
    }
}

fn brief_request(system: &str, input: &str) -> Value {
    json!({
        "model": MODEL,
        "messages": [
            {"role":"system","content":system},
            {"role":"user","content":input},
        ],
        "reasoning_effort": "instant",
        "max_completion_tokens": 128,
    })
}

fn completed_message(payload: &Value) -> Result<String> {
    let choice = &payload["choices"][0];
    let message = &choice["message"];
    match choice["finish_reason"].as_str() {
        Some("content_filter") => return Err(ProviderFailure::Refused.into()),
        Some("length") => return Err(ProviderFailure::OutputLimit.into()),
        Some("stop") => {}
        _ => return Err(ProviderFailure::Incomplete.into()),
    }
    ensure!(message["role"] == "assistant", ProviderFailure::Incomplete);
    ensure!(
        message["refusal"].is_null() || message["refusal"] == "",
        ProviderFailure::Refused
    );
    ensure!(
        message["tool_calls"].is_null()
            || message["tool_calls"].as_array().is_some_and(Vec::is_empty),
        ProviderFailure::Incomplete
    );
    let text = message["content"]
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .ok_or(ProviderFailure::Incomplete)?;
    Ok(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_a_small_instant_completion_without_tools() {
        let payload = brief_request("Be brief", "Look up the opening hours");
        assert_eq!(payload["model"], "mercury-3");
        assert_eq!(payload["reasoning_effort"], "instant");
        assert_eq!(payload["max_completion_tokens"], 128);
        assert_eq!(
            payload["messages"][1]["content"],
            "Look up the opening hours"
        );
        assert!(payload.get("tools").is_none());
    }

    #[test]
    fn accepts_completed_text_and_rejects_partial_or_non_reply_output() {
        let mut payload = json!({"choices":[{"finish_reason":"stop", "message":{
            "role":"assistant", "content":"Ik zoek de openingstijden voor je op."
        }}]});
        assert_eq!(
            completed_message(&payload).unwrap(),
            "Ik zoek de openingstijden voor je op."
        );
        payload["choices"][0]["finish_reason"] = json!("length");
        assert_eq!(
            completed_message(&payload)
                .unwrap_err()
                .downcast::<ProviderFailure>()
                .unwrap(),
            ProviderFailure::OutputLimit
        );
        payload["choices"][0]["finish_reason"] = json!("stop");
        payload["choices"][0]["message"]["tool_calls"] = json!([{"id":"fixture"}]);
        assert!(completed_message(&payload).is_err());
    }
}
