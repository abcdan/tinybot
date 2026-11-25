use super::Step;
use crate::{
    audit,
    model::{ProviderFailure, parse_json_reply},
    net,
};
use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextReply {
    text: String,
}

pub(super) async fn turn(response: &str) -> Result<Step> {
    if let Ok(step) = parse_json_reply(response) {
        return Ok(step);
    }
    let reply = unwrapped(response).and_then(|(text, format)| {
        ensure!(!text.trim().is_empty(), ProviderFailure::Incomplete);
        Ok((text, format))
    });
    let (status, format) = match &reply {
        Ok((_, format)) => ("accepted", *format),
        Err(_) => ("rejected", "unreadable"),
    };
    audit::record(
        "model",
        "reply_format",
        status,
        json!({"format":format,"response_bytes":response.len()}),
    )
    .await;
    let (text, _) = reply?;
    Ok(Step::Reply {
        text,
        suggested_replies: Value::Null,
    })
}

pub(super) fn text(response: &str, allow_empty: bool) -> Result<String> {
    let text = match parse_json_reply::<Step>(response) {
        Ok(Step::Reply { text, .. }) => text,
        Ok(Step::Tool { .. }) => return Err(ProviderFailure::Incomplete.into()),
        Err(_) => unwrapped(response)?.0,
    };
    ensure!(
        !text.trim().is_empty() || allow_empty,
        ProviderFailure::Incomplete
    );
    Ok(text)
}

fn unwrapped(response: &str) -> Result<(String, &'static str)> {
    if let Ok(reply) = parse_json_reply::<TextReply>(response) {
        return Ok((reply.text, "text_object"));
    }
    let text = response.trim();
    let link_led = text
        .strip_prefix('[')
        .and_then(|rest| rest.split_once("]("))
        .and_then(|(_, target)| target.split_once(')'))
        .is_some_and(|(url, _)| url.starts_with("https://") && net::website(url).is_ok());
    ensure!(
        text.len() <= 8_000
            && !text.starts_with(['{', '"', '<'])
            && (!text.starts_with('[') || link_led)
            && !text.starts_with("```")
            && !matches!(text, "null" | "true" | "false")
            && text.chars().any(char::is_alphanumeric)
            && !has_embedded_envelope(text),
        ProviderFailure::Incomplete
    );
    Ok((text.to_owned(), "plain_text"))
}

fn has_embedded_envelope(text: &str) -> bool {
    // A prefixed or fenced model envelope must never be delivered as ordinary text.
    const FIELDS: &[&str] = &[
        "kind",
        "text",
        "tool_calls",
        "function_call",
        "args",
        "arguments",
        "error",
    ];
    if text.contains('{')
        && text.split(['{', ',']).skip(1).any(|field| {
            field
                .trim_start()
                .strip_prefix('"')
                .and_then(|field| field.split_once('"'))
                .is_some_and(|(field, tail)| {
                    FIELDS.contains(&field) && tail.trim_start().starts_with(':')
                })
        })
    {
        return true;
    }
    let lower = text.to_ascii_lowercase();
    ["<tool", "<function", "<think", "<analysis", "<reasoning"]
        .iter()
        .any(|tag| lower.contains(tag))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn complete_prose_or_a_text_object_can_only_become_a_reply() {
        for response in [
            "Het begint om tien uur.",
            r#"{"text":"Het begint om tien uur."}"#,
        ] {
            let step = turn(response).await.unwrap();
            assert!(
                matches!(step, Step::Reply {text, suggested_replies: Value::Null}
                if text == "Het begint om tien uur.")
            );
        }
        for response in [
            r#"{"kind":"reply","text":"Incomplete answer"#,
            r#"{"text":"Extra data","arguments":{}}"#,
            "Calling a tool now:\n```json\n{\"kind\":\"tool\",\"name\":\"browse\"}\n```",
            "Calling: { \"name\": \"browse\", \"kind\": \"tool\" }",
            "Just a moment. <ThInK>Internal deliberation</think>",
            "Result follows:\n{ \"text\": \"Incomplete",
        ] {
            assert!(turn(response).await.is_err(), "{response}");
            assert!(text(response, false).is_err());
        }
    }
}
