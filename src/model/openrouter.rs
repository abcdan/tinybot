use super::{ModelProvider, TextDelta};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::StreamExt;
use rig::{
    agent::{Agent, MultiTurnStreamItem},
    completion::Prompt,
    prelude::CompletionClient,
    providers::openrouter,
    streaming::{StreamedAssistantContent, StreamingPrompt},
};
use std::time::Duration;

pub(super) struct OpenRouter {
    pub(super) key: String,
    pub(super) model: String,
}

impl OpenRouter {
    fn agent(&self, system: &str) -> Agent<openrouter::CompletionModel> {
        openrouter::Client::new(&self.key)
            .agent(&self.model)
            .preamble(system)
            .max_tokens(3600)
            .build()
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
                    MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text)) => {
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
}
