use super::{ModelProvider, TextDelta};
use anyhow::{Context, Result, anyhow, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdout, Command},
};

pub(super) struct Codex {
    pub(super) access: String,
    pub(super) model: String,
    pub(super) binary: String,
}

#[async_trait]
impl ModelProvider for Codex {
    async fn complete(&self, system: &str, input: &str) -> Result<String> {
        tokio::time::timeout(Duration::from_secs(100), self.run(system, input, None)).await?
    }

    async fn complete_streaming(
        &self,
        system: &str,
        input: &str,
        delta: &mut TextDelta<'_>,
    ) -> Result<String> {
        tokio::time::timeout(Duration::from_secs(100), self.run(system, input, Some(delta))).await?
    }
}

impl Codex {
    async fn run(
        &self,
        system: &str,
        input: &str,
        mut delta: Option<&mut TextDelta<'_>>,
    ) -> Result<String> {
        let mut command = Command::new(&self.binary);
        command.args([
            "app-server",
            "--listen",
            "stdio://",
            "-c",
            "model_provider=\"tinybot_chatgpt\"",
            "-c",
            "model_providers.tinybot_chatgpt.name=\"ChatGPT plan\"",
            "-c",
            "model_providers.tinybot_chatgpt.base_url=\"https://api.openai.com/v1\"",
            "-c",
            "model_providers.tinybot_chatgpt.env_key=\"TINYBOT_CODEX_ACCESS\"",
            "-c",
            "model_providers.tinybot_chatgpt.wire_api=\"responses\"",
            "-c",
            "model_providers.tinybot_chatgpt.requires_openai_auth=false",
            "-c",
            "model_providers.tinybot_chatgpt.supports_websockets=false",
            "-c",
            "tools.disable_defaults=true",
            "-c",
            "features.shell_tool=false",
            "-c",
            "features.unified_exec=false",
            "-c",
            "features.plugins=false",
            "-c",
            "features.multi_agent=false",
            "-c",
            "web_search=\"disabled\"",
            "-c",
            "approval_policy=\"never\"",
            "-c",
            "sandbox_mode=\"read-only\"",
        ]);
        let home =
            std::env::var("TINYBOT_CODEX_HOME").unwrap_or_else(|_| "/tmp/tinybot-codex".into());
        tokio::fs::create_dir_all(&home).await?;
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &home)
            .env("CODEX_HOME", &home)
            .env("TINYBOT_CODEX_ACCESS", &self.access)
            .current_dir(&home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().context("Codex App Server is unavailable")?;
        let mut lines =
            BufReader::new(child.stdout.take().context("missing Codex output")?).lines();
        rpc(&mut child, json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"tinybot","title":"TinyBot","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}})).await?;
        wait_result(&mut lines, 1).await?;
        rpc(&mut child, json!({"method":"initialized"})).await?;
        rpc(&mut child,json!({"id":2,"method":"thread/start","params":{"model":self.model,"ephemeral":true,"environments":[],"baseInstructions":system,"approvalPolicy":"never","sandbox":"read-only"}})).await?;
        let thread = wait_result(&mut lines, 2).await?;
        let thread = thread["thread"]["id"]
            .as_str()
            .context("Codex did not create a thread")?;
        rpc(&mut child,json!({"id":3,"method":"turn/start","params":{"threadId":thread,"environments":[],"input":[{"type":"text","text":input}]}})).await?;
        let mut result = String::new();
        let mut preview = AnswerItem::default();
        while let Some(line) = lines.next_line().await? {
            let value: Value = serde_json::from_str(&line)?;
            if value.get("method").is_some() && value.get("id").is_some() {
                rpc(&mut child,json!({"id":value["id"],"error":{"code":-32601,"message":"Native tools are disabled in TinyBot"}})).await?;
            }
            ensure!(
                value["id"] != 3 || value.get("error").is_none(),
                "Codex rejected the turn"
            );
            if let Some(delta) = delta.as_deref_mut()
                && let Some(text) = preview.update(thread, &value)
            {
                delta(text);
            }
            match value["method"].as_str() {
                Some("item/completed") => {
                    let item = &value["params"]["item"];
                    if item["type"] == "agentMessage" && item["phase"] != "commentary" {
                        if let Some(text) = item["text"].as_str() {
                            result = text.to_owned();
                        }
                    }
                }
                Some("turn/completed") => {
                    ensure!(
                        value["params"]["turn"]["status"] == "completed" && !result.is_empty(),
                        "Codex did not finish the turn"
                    );
                    return Ok(result);
                }
                _ => {}
            }
        }
        Err(anyhow!(
            "Codex App Server stopped before completing the turn"
        ))
    }
}

#[derive(Default)]
struct AnswerItem {
    id: Option<String>,
    completed: bool,
}

impl AnswerItem {
    fn update<'a>(&mut self, thread: &str, notification: &'a Value) -> Option<&'a str> {
        let params = &notification["params"];
        if params["threadId"] != thread || self.completed {
            return None;
        }
        let item = &params["item"];
        match notification["method"].as_str()? {
            "item/started"
                if self.id.is_none()
                    && item["type"] == "agentMessage"
                    && item["phase"] == "final_answer" =>
            {
                self.id = Some(item["id"].as_str()?.to_owned());
                item["text"].as_str()
            }
            "item/agentMessage/delta"
                if self.id.as_deref().is_some_and(|id| params["itemId"] == id) =>
            {
                params["delta"].as_str()
            }
            "item/completed" if self.id.as_deref().is_some_and(|id| item["id"] == id) => {
                self.completed = true;
                None
            }
            _ => None,
        }
    }
}

async fn rpc(child: &mut Child, message: Value) -> Result<()> {
    let stdin = child.stdin.as_mut().context("missing Codex input")?;
    stdin.write_all(message.to_string().as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}

async fn wait_result(lines: &mut Lines<BufReader<ChildStdout>>, id: u64) -> Result<Value> {
    while let Some(line) = lines.next_line().await? {
        let value: Value = serde_json::from_str(&line)?;
        if value["id"] == id {
            return value
                .get("result")
                .cloned()
                .context("Codex App Server rejected initialization");
        }
    }
    Err(anyhow!("Codex App Server disconnected"))
}
