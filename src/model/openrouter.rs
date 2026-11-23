use super::{ModelProvider, TextDelta, ToolDefinition, native_reply, native_tool_proposal};
use anyhow::{Context, Result, anyhow, ensure};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use rig::{
    agent::{Agent, MultiTurnStreamItem},
    completion::{AssistantContent, CompletionModel, Prompt, ToolDefinition as RigToolDefinition},
    prelude::CompletionClient,
    providers::openrouter,
    streaming::{StreamedAssistantContent, StreamingPrompt},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};

const MAX_TOKENS: u64 = 3600;
const MAX_STREAM_BYTES: usize = 1_000_000;
const CHAT_COMPLETIONS: &str = "https://openrouter.ai/api/v1/chat/completions";

pub(super) struct OpenRouter {
    pub(super) key: String,
    pub(super) model: String,
}

impl OpenRouter {
    fn agent(&self, system: &str) -> Agent<openrouter::CompletionModel> {
        openrouter::Client::new(&self.key)
            .agent(&self.model)
            .preamble(system)
            .max_tokens(MAX_TOKENS)
            .build()
    }

    async fn native_buffered(
        &self,
        system: &str,
        input: &str,
        tools: &[ToolDefinition],
    ) -> Result<String> {
        let client = openrouter::Client::new(&self.key);
        let model = client.completion_model(&self.model);
        let response = model
            .completion_request(input)
            .preamble(system.to_owned())
            .tools(rig_tools(tools))
            .additional_params(json!({
                "tool_choice":"auto",
                "parallel_tool_calls":false,
                "max_tokens":MAX_TOKENS,
            }))
            .send()
            .await
            .map_err(|_| anyhow!("OpenRouter could not complete the tool turn"))?;
        let finish = response
            .raw_response
            .choices
            .first()
            .and_then(|choice| choice.finish_reason.as_deref());
        let mut text = String::new();
        for item in response.choice {
            match item {
                AssistantContent::Text(part) if finish == Some("stop") => text.push_str(&part.text),
                AssistantContent::ToolCall(call)
                    if finish == Some("tool_calls") && !call.id.is_empty() =>
                {
                    if let Ok(proposal) =
                        native_tool_proposal(tools, &call.function.name, call.function.arguments)
                    {
                        return Ok(proposal);
                    }
                }
                _ => {}
            }
        }
        ensure!(
            !text.trim().is_empty(),
            "OpenRouter did not complete a reply or tool proposal"
        );
        native_reply(text)
    }

    async fn native_streamed(
        &self,
        system: &str,
        input: &str,
        tools: &[ToolDefinition],
        delta: &mut TextDelta<'_>,
    ) -> Result<String> {
        let response = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .build()?
            .post(CHAT_COMPLETIONS)
            .bearer_auth(&self.key)
            .json(&stream_request(&self.model, system, input, tools))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|_| anyhow!("OpenRouter could not stream the tool turn"))?;
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

        // Rig 0.24's OpenRouter stream does not retain finish reasons and rejects
        // usage-only frames. Require a finish reason before handing a call to the broker.
        let mut turn = StreamTurn::default();
        while let Some(event) = events.next().await {
            let event = event.map_err(|_| anyhow!("OpenRouter stream failed"))?;
            if event.data == "[DONE]" {
                break;
            }
            if let Some(result) = turn.accept(&event.data, tools, delta)? {
                return Ok(result);
            }
        }
        Err(anyhow!(
            "OpenRouter stream ended without a completed reply or tool proposal"
        ))
    }
}

#[async_trait]
impl ModelProvider for OpenRouter {
    async fn complete(&self, system: &str, input: &str) -> Result<String> {
        let agent = self.agent(system);
        tokio::time::timeout(Duration::from_secs(90), agent.prompt(input))
            .await?
            .map_err(|_| anyhow!("OpenRouter could not complete the request"))
    }

    async fn complete_streaming(
        &self,
        system: &str,
        input: &str,
        delta: &mut TextDelta<'_>,
    ) -> Result<String> {
        tokio::time::timeout(Duration::from_secs(90), async {
            let agent = self.agent(system);
            let mut stream = agent.stream_prompt(input).await;
            while let Some(item) = stream.next().await {
                match item.map_err(|_| anyhow!("OpenRouter could not complete the request"))? {
                    MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(
                        text,
                    )) => {
                        delta(&text.text);
                    }
                    MultiTurnStreamItem::FinalResponse(response) => {
                        return Ok(response.response().to_owned());
                    }
                    _ => {}
                }
            }
            Err(anyhow!("OpenRouter stopped before completing the request"))
        })
        .await?
    }

    async fn complete_turn(
        &self,
        system: &str,
        input: &str,
        tools: &[ToolDefinition],
        delta: Option<&mut TextDelta<'_>>,
    ) -> Result<String> {
        if tools.is_empty() {
            return match delta {
                Some(delta) => self.complete_streaming(system, input, delta).await,
                None => self.complete(system, input).await,
            };
        }
        let timeout = Duration::from_secs(90);
        match delta {
            Some(delta) => {
                tokio::time::timeout(timeout, self.native_streamed(system, input, tools, delta))
                    .await?
            }
            None => {
                tokio::time::timeout(timeout, self.native_buffered(system, input, tools)).await?
            }
        }
    }
}

fn rig_tools(tools: &[ToolDefinition]) -> Vec<RigToolDefinition> {
    tools
        .iter()
        .map(|tool| RigToolDefinition {
            name: tool.name.into(),
            description: tool.description.into(),
            parameters: tool.input_schema.clone(),
        })
        .collect()
}

fn stream_request(model: &str, system: &str, input: &str, tools: &[ToolDefinition]) -> Value {
    let tools: Vec<_> = rig_tools(tools)
        .into_iter()
        .map(|tool| json!({"type":"function","function":tool}))
        .collect();
    json!({
        "model":model,
        "messages":[{"role":"system","content":system},{"role":"user","content":input}],
        "stream":true,
        "tools":tools,
        "tool_choice":"auto",
        "parallel_tool_calls":false,
        "max_tokens":MAX_TOKENS,
    })
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
        data: &str,
        tools: &[ToolDefinition],
        delta: &mut TextDelta<'_>,
    ) -> Result<Option<String>> {
        let event: Value = serde_json::from_str(data)?;
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
