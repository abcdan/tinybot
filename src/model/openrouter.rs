use super::{
    AttachmentInput, AttachmentRead, ModelProvider, TextDelta, ToolDefinition,
    media::{READ_ATTACHMENT, data_url, read_report},
    native_reply, native_tool_proposal,
};
use crate::{
    audit::{self, ModelCall},
    net,
};
use anyhow::{Context, Result, anyhow, ensure};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
use tokio::time::{Instant, timeout_at};

const MAX_TOKENS: u64 = 3600;
const MAX_STREAM_BYTES: usize = 1_000_000;
const CHAT_COMPLETIONS: &str = "https://openrouter.ai/api/v1/chat/completions";

pub(super) struct OpenRouter {
    pub(super) key: String,
    pub(super) model: String,
}

impl OpenRouter {
    async fn send(&self, request: Value) -> Result<reqwest::Response> {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .build()?
            .post(CHAT_COMPLETIONS)
            .bearer_auth(&self.key)
            .json(&request)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|_| anyhow!("OpenRouter could not complete the request"))
    }
}

#[async_trait]
impl ModelProvider for OpenRouter {
    async fn complete(&self, system: &str, input: &str) -> Result<String> {
        self.complete_turn(system, input, &[], None).await
    }

    async fn read_attachment(
        &self,
        input: &str,
        attachment: AttachmentInput<'_>,
    ) -> Result<Option<AttachmentRead>> {
        audit::provider_call("openrouter", &self.model, async |call| {
            let request = attachment_request(&self.model, input, attachment);
            let deadline = Instant::now() + Duration::from_secs(60);
            let response = timeout_at(deadline, self.send(request)).await??;
            let reply = read_buffered(response, &[], call, deadline).await?;
            Ok(Some(read_report(&reply)?))
        })
        .await
    }

    async fn complete_streaming(
        &self,
        system: &str,
        input: &str,
        delta: &mut TextDelta<'_>,
    ) -> Result<String> {
        self.complete_turn(system, input, &[], Some(delta)).await
    }

    async fn complete_turn(
        &self,
        system: &str,
        input: &str,
        tools: &[ToolDefinition],
        delta: Option<&mut TextDelta<'_>>,
    ) -> Result<String> {
        audit::provider_call("openrouter", &self.model, async |call| {
            let deadline = Instant::now() + Duration::from_secs(90);
            let req = chat_request(&self.model, system, input, tools, delta.is_some());
            let response = timeout_at(deadline, self.send(req)).await??;
            match delta {
                Some(delta) => stream_response(response, tools, delta, call, deadline).await,
                None => read_buffered(response, tools, call, deadline).await,
            }
        })
        .await
    }
}

async fn read_buffered(
    response: reqwest::Response,
    tools: &[ToolDefinition],
    call: &mut ModelCall,
    deadline: Instant,
) -> Result<String> {
    let body = timeout_at(deadline, net::body(response, MAX_STREAM_BYTES)).await??;
    let reply: Value =
        serde_json::from_slice(&body).context("invalid OpenRouter completion response")?;
    buffered_response(&reply, tools, call)
}

fn buffered_response(
    reply: &Value,
    tools: &[ToolDefinition],
    call: &mut ModelCall,
) -> Result<String> {
    call.openrouter(reply);
    let choice = &reply["choices"][0];
    let message = &choice["message"];
    ensure!(
        message["role"] == "assistant",
        "OpenRouter did not return an assistant message"
    );
    let finish = choice["finish_reason"].as_str();
    if finish == Some("tool_calls") {
        for item in message["tool_calls"].as_array().into_iter().flatten() {
            if let Some(kind) = item.get("type")
                && kind != "function"
            {
                continue;
            }
            if let (Some(name), Some(args)) = (
                item["function"]["name"].as_str(),
                item["function"]["arguments"].as_str(),
            ) && item["id"].as_str().is_some_and(|id| !id.is_empty())
                && let Ok(args) = serde_json::from_str(args)
                && let Ok(proposal) = native_tool_proposal(tools, name, args)
            {
                return Ok(proposal);
            }
        }
    }
    let content = &message["content"];
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match part["type"].as_str() {
                Some("text") => part["text"].as_str(),
                Some("refusal") => part["refusal"].as_str(),
                _ => None,
            })
            .collect::<String>(),
        _ => message["refusal"].as_str().unwrap_or_default().to_owned(),
    };
    ensure!(
        (tools.is_empty() || finish == Some("stop")) && !text.trim().is_empty(),
        "OpenRouter did not complete a reply or tool proposal"
    );
    if tools.is_empty() {
        Ok(text)
    } else {
        native_reply(text)
    }
}

async fn stream_response(
    response: reqwest::Response,
    tools: &[ToolDefinition],
    delta: &mut TextDelta<'_>,
    call: &mut ModelCall,
    deadline: Instant,
) -> Result<String> {
    let mut bytes = 0;
    let mut events = response
        .bytes_stream()
        .map(move |chunk| {
            let chunk = chunk?;
            bytes += chunk.len();
            ensure!(
                bytes <= MAX_STREAM_BYTES,
                "OpenRouter stream exceeded its size limit"
            );
            Ok::<_, anyhow::Error>(chunk)
        })
        .eventsource();
    let mut turn = StreamTurn::default();
    let mut result = None;
    let mut accounting_deadline = None;
    loop {
        let event = if let Some(deadline) = accounting_deadline {
            // Usage arrives after the finish frame. Bound the wait when a relay
            // omits the final usage frame or leaves the connection open.
            match timeout_at(deadline, events.next()).await {
                Ok(event) => event,
                Err(_) => break,
            }
        } else {
            timeout_at(deadline, events.next()).await?
        };
        let Some(event) = event else { break };
        let event = match event {
            Ok(event) => event,
            Err(_) if result.is_some() => break,
            Err(_) => return Err(anyhow!("OpenRouter stream failed")),
        };
        if event.data == "[DONE]" {
            break;
        }
        let value: Value = match serde_json::from_str(&event.data) {
            Ok(value) => value,
            Err(_) if result.is_some() => continue,
            Err(_) => return Err(anyhow!("invalid OpenRouter stream frame")),
        };
        call.openrouter(&value);
        if result.is_some()
            && value["usage"].is_object()
            && value["choices"].as_array().is_none_or(Vec::is_empty)
        {
            break;
        }
        if result.is_none() {
            result = turn.accept(&value, tools, delta).transpose();
            if result.is_some() {
                accounting_deadline = Some(Instant::now() + Duration::from_secs(2));
            }
        }
    }
    result.context("OpenRouter stream ended without a completed reply or tool proposal")?
}

fn chat_request(
    model: &str,
    system: &str,
    input: &str,
    tools: &[ToolDefinition],
    stream: bool,
) -> Value {
    let mut body = json!({
        "model":model,
        "messages":[{"role":"system","content":system},{"role":"user","content":input}],
        "stream":stream,
        "usage":{"include":true},
    });
    if stream {
        body["stream_options"] = json!({"include_usage":true});
    }
    if !tools.is_empty() {
        body["max_tokens"] = json!(MAX_TOKENS);
        body["tools"] =
            json!(tools.iter().map(|tool| json!({"type":"function","function":{
            "name":tool.name,"description":tool.description,"parameters":tool.input_schema
        }})).collect::<Vec<_>>());
        body["tool_choice"] = json!("auto");
        body["parallel_tool_calls"] = json!(false);
    }
    body
}

fn attachment_request(model: &str, input: &str, attachment: AttachmentInput<'_>) -> Value {
    let mut body = chat_request(model, READ_ATTACHMENT, input, &[], false);
    let part = match attachment {
        AttachmentInput::Image { mime, bytes } => {
            json!({"type":"image_url","image_url":{"url":data_url(mime, bytes)}})
        }
        AttachmentInput::Pdf { name, bytes } => json!({"type":"file","file":{
            "filename":name,"file_data":data_url("application/pdf", bytes)
        }}),
    };
    body["messages"][1]["content"] = json!([{"type":"text","text":input}, part]);
    body["max_tokens"] = json!(MAX_TOKENS);
    body
}

#[derive(Default)]
struct StreamCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct StreamTurn {
    text: String,
    calls: BTreeMap<u64, StreamCall>,
}

impl StreamTurn {
    fn accept(
        &mut self,
        event: &Value,
        tools: &[ToolDefinition],
        delta: &mut TextDelta<'_>,
    ) -> Result<Option<String>> {
        ensure!(
            event["error"].is_null(),
            "OpenRouter reported a stream error"
        );
        let Some(choice) = event["choices"]
            .as_array()
            .and_then(|choices| choices.first())
        else {
            // Providers may send usage-only frames without a choice.
            return Ok(None);
        };
        let update = &choice["delta"];
        if let Some(calls) = update["tool_calls"].as_array() {
            for call in calls {
                let index = call["index"]
                    .as_u64()
                    .context("missing OpenRouter tool index")?;
                ensure!(index < 16, "too many OpenRouter tool proposals");
                let pending = self.calls.entry(index).or_default();
                if let Some(id) = call["id"].as_str() {
                    pending.id = id.to_owned();
                }
                if let Some(name) = call["function"]["name"].as_str() {
                    pending.name.push_str(name);
                }
                if let Some(arguments) = call["function"]["arguments"].as_str() {
                    pending.arguments.push_str(arguments);
                }
            }
        }
        if let Some(text) = update["content"].as_str()
            && self.calls.is_empty()
        {
            self.text.push_str(text);
            delta(text);
        }
        match choice["finish_reason"].as_str() {
            None => Ok(None),
            Some("stop" | "length") if tools.is_empty() && !self.text.trim().is_empty() => {
                Ok(Some(std::mem::take(&mut self.text)))
            }
            Some("stop") if self.calls.is_empty() && !self.text.trim().is_empty() => {
                native_reply(std::mem::take(&mut self.text)).map(Some)
            }
            Some("tool_calls") => {
                for call in self.calls.values().filter(|call| !call.id.is_empty()) {
                    if let Ok(args) = serde_json::from_str(&call.arguments)
                        && let Ok(proposal) = native_tool_proposal(tools, &call.name, args)
                    {
                        return Ok(Some(proposal));
                    }
                }
                Err(anyhow!("OpenRouter did not complete a valid tool proposal"))
            }
            Some(_) => Err(anyhow!("OpenRouter did not finish the tool turn")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definitions() -> [ToolDefinition; 1] {
        [ToolDefinition {
            name: "browse",
            description: "Read a public website.",
            input_schema: json!({
                "type":"object","properties":{"url":{"type":"string"}},"required":["url"]
            }),
        }]
    }

    #[test]
    fn registers_native_openrouter_function_schemas() {
        let definitions = definitions();
        let body = chat_request(
            "provider/model",
            "system prompt",
            "task context",
            &definitions,
            true,
        );
        assert_eq!(body["messages"][0]["content"], "system prompt");
        assert_eq!(body["messages"][1]["content"], "task context");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "browse");
        assert_eq!(
            body["tools"][0]["function"]["parameters"],
            definitions[0].input_schema
        );
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["max_tokens"], MAX_TOKENS);
        assert_eq!(body["usage"]["include"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        let auxiliary = chat_request("inception/mercury-3", "preamble", "input", &[], false);
        assert_eq!(auxiliary["usage"]["include"], true);
        assert!(auxiliary.get("tools").is_none());
        assert!(auxiliary.get("max_tokens").is_none());
    }

    #[test]
    fn attachment_requests_use_native_parts_without_tools() {
        let image = attachment_request(
            "provider/model",
            "Read the sign",
            AttachmentInput::Image {
                mime: "image/png",
                bytes: b"fixture",
            },
        );
        assert_eq!(image["messages"][1]["content"][0]["text"], "Read the sign");
        assert_eq!(
            image["messages"][1]["content"][1],
            json!({
                "type":"image_url","image_url":{"url":"data:image/png;base64,Zml4dHVyZQ=="}
            })
        );
        assert!(image.get("tools").is_none());
        let file = attachment_request(
            "provider/model",
            "Review this file",
            AttachmentInput::Pdf {
                name: "fixture.pdf",
                bytes: b"fixture",
            },
        );
        assert_eq!(
            file["messages"][1]["content"][1],
            json!({"type":"file","file":{
                "filename":"fixture.pdf","file_data":"data:application/pdf;base64,Zml4dHVyZQ=="
            }})
        );
        assert!(file.get("tools").is_none());
    }

    #[tokio::test]
    async fn tallies_the_usage_tail_after_the_stream_finish_frame() {
        use axum::{Router, http::header, routing::get};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/fixture", listener.local_addr().unwrap());
        let app = Router::new().route("/fixture", get(|| async {
            ([(header::CONTENT_TYPE, "text/event-stream")], r#"data: {"provider":"Fixture Compute","model":"fixture/served","choices":[{"delta":{"content":"Hello"},"finish_reason":"stop"}]}

data: {"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10,"cost":0.00001,"private_field":"not for audit"}}

data: [DONE]

"#)
        }));
        let worker = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(endpoint)
            .send()
            .await
            .unwrap();
        audit::provider_call("openrouter", "openrouter/free", async |call| {
            let mut draft = String::new();
            let output = stream_response(response, &definitions(), &mut |part| {
                draft.push_str(part);
            }, call, Instant::now() + Duration::from_secs(2)).await?;
            assert_eq!(output, "Hello");
            assert_eq!(draft, "Hello");
            assert_eq!(json!(call), json!({
                "provider":"Fixture Compute","model":"fixture/served",
                "usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10,"cost_usd":0.00001}
            }));
            Ok(())
        }).await.unwrap();
        worker.abort();
    }

    #[tokio::test]
    async fn buffered_calls_preserve_routed_usage_and_native_proposals() {
        let reply = json!({
            "provider":"Fixture Compute","model":"fixture/served",
            "choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":null,"tool_calls":[
                {"id":"call_1","type":"function","function":{"name":"browse","arguments":"{\"url\":\"https://example.org\"}"}}
            ]}}],
            "usage":{"prompt_tokens":5,"total_tokens":9,"cost":0.0}
        });
        audit::provider_call("openrouter", "fixture/requested", async |call| {
            let output = buffered_response(&reply, &definitions(), call)?;
            assert_eq!(
                serde_json::from_str::<Value>(&output)?,
                json!({
                    "kind":"tool","name":"browse","args":{"url":"https://example.org"}
                })
            );
            assert_eq!(
                json!(call),
                json!({
                    "provider":"Fixture Compute","model":"fixture/served",
                    "usage":{"input_tokens":5,"output_tokens":null,"total_tokens":9,"cost_usd":0.0}
                })
            );
            let mut invalid = reply;
            invalid["choices"][0]["message"]["tool_calls"][0]["type"] = json!("other");
            assert!(buffered_response(&invalid, &definitions(), call).is_err());
            invalid["choices"][0]["message"]["tool_calls"][0]["type"] = json!("function");
            invalid["choices"][0]["message"]["role"] = json!("user");
            assert!(buffered_response(&invalid, &definitions(), call).is_err());
            Ok(())
        })
        .await
        .unwrap();
    }

    #[test]
    fn waits_for_the_tool_finish_frame_and_keeps_arguments_out_of_drafts() {
        let definitions = definitions();
        let mut turn = StreamTurn::default();
        let mut draft = String::new();
        let mut emit = |part: &str| draft.push_str(part);
        assert!(
            turn.accept(
                &json!({"choices":[],"usage":{"completion_tokens":4}}),
                &definitions,
                &mut emit
            )
            .unwrap()
            .is_none()
        );
        let start = json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"call_a","function":{"name":"browse","arguments":"{\"url\":"}}
        ]},"finish_reason":null}]});
        let args = json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"\"https://example.org\"}"}}
        ]},"finish_reason":null}]});
        assert!(
            turn.accept(&start, &definitions, &mut emit)
                .unwrap()
                .is_none()
        );
        assert!(
            turn.accept(&args, &definitions, &mut emit)
                .unwrap()
                .is_none()
        );
        let result = turn
            .accept(
                &json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
                &definitions,
                &mut emit,
            )
            .unwrap()
            .unwrap();
        assert!(draft.is_empty());
        assert_eq!(
            serde_json::from_str::<Value>(&result).unwrap(),
            json!({
                "kind":"tool","name":"browse","args":{"url":"https://example.org"}
            })
        );
    }

    #[test]
    fn streams_reply_content_and_requires_the_native_wire_for_tools() {
        let definitions = definitions();
        let mut turn = StreamTurn::default();
        let mut emit = |_: &str| {};
        let reply = r#"{"kind":"reply","text":"Hello"}"#;
        let finished = json!({"choices":[{"delta":{"content":reply},"finish_reason":"stop"}]});
        let mut draft = String::new();
        assert_eq!(
            turn.accept(&finished, &definitions, &mut |part: &str| draft
                .push_str(part))
                .unwrap(),
            Some(reply.into())
        );
        assert_eq!(draft, reply);
        let fake = json!({"choices":[{"delta":{"content":"{\"kind\":\"tool\",\"name\":\"browse\",\"args\":{}}"},"finish_reason":"stop"}]});
        assert!(turn.accept(&fake, &definitions, &mut emit).is_err());
        assert!(
            turn.accept(
                &json!({"error":{"message":"partial failure"}}),
                &definitions,
                &mut emit
            )
            .is_err()
        );
    }
}
