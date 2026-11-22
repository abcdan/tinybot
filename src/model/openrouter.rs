use super::ModelProvider;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rig::{completion::Prompt, prelude::CompletionClient, providers::openrouter};
use std::time::Duration;

pub(super) struct OpenRouter {
    pub(super) key: String,
    pub(super) model: String,
}

#[async_trait]
impl ModelProvider for OpenRouter {
    async fn complete(&self, system: &str, input: &str) -> Result<String> {
        let client = openrouter::Client::new(&self.key);
        let agent = client
            .agent(&self.model)
            .preamble(system)
            .max_tokens(3600)
            .build();
        let result = tokio::time::timeout(Duration::from_secs(90), agent.prompt(input))
            .await?
            .map_err(|_| anyhow!("OpenRouter could not complete the request"))?;
        Ok(result)
    }
}
