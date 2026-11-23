use super::{
    AttachmentInput, AttachmentRead, ModelProvider, TextDelta, ToolDefinition,
    media::{READ_ATTACHMENT, data_url, read_pdf_text, read_report},
    native_reply, native_tool_proposal,
};
use crate::audit::{self, ModelCall};
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
        self.complete_turn(system, input, &[], None).await
    }

    async fn read_attachment(
        &self,
        input: &str,
        attachment: AttachmentInput<'_>,
    ) -> Result<Option<AttachmentRead>> {
        let url = match attachment {
            AttachmentInput::Image { mime, bytes } => data_url(mime, bytes),
            AttachmentInput::Pdf { bytes, .. } => return Ok(Some(read_pdf_text(bytes).await?)),
        };
        audit::provider_call("codex", &self.model, async |call| {
            let input = turn_input(input, Some(url));
            let reply = tokio::time::timeout(
                Duration::from_secs(60),
                self.run(READ_ATTACHMENT, input, &[], None, call),
            )
            .await??;
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
        audit::provider_call("codex", &self.model, async |call| {
            tokio::time::timeout(
                Duration::from_secs(100),
                self.run(system, turn_input(input, None), tools, delta, call),
            )
            .await?
        })
        .await
    }
}

impl Codex {
    async fn run(
        &self,
        system: &str,
        input: Vec<Value>,
        tools: &[ToolDefinition],
        mut delta: Option<&mut TextDelta<'_>>,
        call: &mut ModelCall,
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
            "features.default_mode_request_user_input=false",
            "-c",
            "features.view_image=false",
            "-c",
            "features.goals=false",
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
        let mut thread_params = json!({
            "model":self.model,"ephemeral":true,"environments":[],"baseInstructions":system,
            "approvalPolicy":"never","sandbox":"read-only"
        });
        if !tools.is_empty() {
            thread_params["dynamicTools"] = dynamic_tools(tools);
        }
        rpc(
            &mut child,
            json!({"id":2,"method":"thread/start","params":thread_params}),
        )
        .await?;
        let thread = wait_result(&mut lines, 2).await?;
        call.model(&thread["model"]);
        let thread = thread["thread"]["id"]
            .as_str()
            .context("Codex did not create a thread")?;
        rpc(&mut child,json!({"id":3,"method":"turn/start","params":{"threadId":thread,"environments":[],"input":input}})).await?;
        let mut result = String::new();
        let mut preview = AnswerItem::default();
        let mut turn_id = None;
        while let Some(line) = lines.next_line().await? {
            let value: Value = serde_json::from_str(&line)?;
            if value.get("id").is_some() {
                if value.get("method").is_some() {
                    if value["method"] == "item/tool/call" && !tools.is_empty() {
                        // This process only proposes calls. The broker executes them after this
                        // ephemeral turn ends and supplies the result in the next turn's history.
                        let params = &value["params"];
                        let result = dynamic_tool_call(tools, thread, turn_id.as_deref(), params);
                        if result.is_ok() && !call.has_tokens() {
                            let tool_turn = params["turnId"].as_str();
                            let _ = tokio::time::timeout(Duration::from_millis(500), async {
                                while let Ok(Some(line)) = lines.next_line().await {
                                    if let Ok(note) = serde_json::from_str(&line) {
                                        observe_turn(call, thread, tool_turn, &note);
                                        if call.has_tokens() {
                                            break;
                                        }
                                    }
                                }
                            })
                            .await;
                        }
                        return result;
                    }
                    rpc(&mut child,json!({"id":value["id"],"error":{"code":-32601,"message":"This Codex tool is unavailable in TinyBot"}})).await?;
                } else if value["id"] == 3 {
                    ensure!(value.get("error").is_none(), "Codex rejected the turn");
                    turn_id = value["result"]["turn"]["id"].as_str().map(str::to_owned);
                }
            }
            observe_turn(call, thread, turn_id.as_deref(), &value);
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
                    return if tools.is_empty() {
                        Ok(result)
                    } else {
                        native_reply(result)
                    };
                }
                _ => {}
            }
        }
        Err(anyhow!(
            "Codex App Server stopped before completing the turn"
        ))
    }
}

fn turn_input(text: &str, image: Option<String>) -> Vec<Value> {
    let mut input = vec![json!({"type":"text","text":text})];
    if let Some(url) = image {
        input.push(json!({"type":"image","url":url}));
    }
    input
}

fn observe_turn(call: &mut ModelCall, thread: &str, turn: Option<&str>, value: &Value) {
    let params = &value["params"];
    if params["threadId"] != thread
        || !params["turnId"]
            .as_str()
            .is_some_and(|id| turn.is_none_or(|turn| id == turn))
    {
        return;
    }
    match value["method"].as_str() {
        Some("thread/tokenUsage/updated") => {
            // Each app server runs one turn in a fresh ephemeral thread. Usage
            // updates are cumulative snapshots for that call, not increments.
            call.codex(&params["tokenUsage"]["total"]);
        }
        Some("model/rerouted") => call.model(&params["toModel"]),
        _ => {}
    }
}

fn dynamic_tools(tools: &[ToolDefinition]) -> Value {
    Value::Array(
        tools
            .iter()
            .map(|tool| {
                json!({
                    "type":"function",
                    "name":tool.name,
                    "description":tool.description,
                    "inputSchema":tool.input_schema
                })
            })
            .collect(),
    )
}

fn dynamic_tool_call(
    tools: &[ToolDefinition],
    thread: &str,
    turn: Option<&str>,
    params: &Value,
) -> Result<String> {
    ensure!(
        params["threadId"] == thread
            && params["turnId"].as_str().is_some()
            && turn.is_none_or(|id| params["turnId"] == id)
            && params["callId"].as_str().is_some_and(|id| !id.is_empty())
            && params["namespace"].is_null(),
        "unexpected Codex tool request"
    );
    let name = params["tool"].as_str().context("missing Codex tool name")?;
    native_tool_proposal(tools, name, params["arguments"].clone())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_turns_carry_images_as_inline_user_input() {
        assert_eq!(
            turn_input("Find the code", Some("data:image/png;base64,cG5n".into())),
            vec![
                json!({"type":"text","text":"Find the code"}),
                json!({"type":"image","url":"data:image/png;base64,cG5n"}),
            ]
        );
    }

    #[tokio::test]
    async fn codex_end_of_turn_usage_is_cumulative_and_keeps_cost_unknown() {
        audit::provider_call("codex", "fixture-requested", async |call| {
            let snapshot = |turn: &str, count: i64| {
                json!({
                    "method":"thread/tokenUsage/updated","params":{
                        "threadId":"thread","turnId":turn,"tokenUsage":{
                            "last":{"inputTokens":1,"outputTokens":1,"totalTokens":2},
                            "total":{"inputTokens":count,"outputTokens":count,"totalTokens":count*2}
                        }
                    }
                })
            };
            observe_turn(call, "thread", Some("valid"), &snapshot("valid", 6));
            observe_turn(call, "thread", Some("valid"), &snapshot("valid", 8));
            observe_turn(call, "thread", Some("valid"), &snapshot("invalid", 99));
            observe_turn(
                call,
                "thread",
                Some("valid"),
                &json!({
                    "method":"model/rerouted","params":{
                        "threadId":"thread","turnId":"valid","toModel":"fixture-actual"
                    }
                }),
            );
            assert_eq!(
                json!(call),
                json!({
                    "provider":"codex","model":"fixture-actual",
                    "usage":{"input_tokens":8,"output_tokens":8,"total_tokens":16,"cost_usd":null}
                })
            );
            Ok(())
        })
        .await
        .unwrap();
    }

    #[test]
    fn codex_dynamic_tool_request_requires_the_registered_function_and_active_turn() {
        let tools = [ToolDefinition {
            name: "request_input",
            description: "Show a form.",
            input_schema: json!({"type":"object","properties":{"purpose":{"type":"string"}}}),
        }];
        assert_eq!(
            dynamic_tools(&tools),
            json!([{
                "type":"function","name":"request_input","description":"Show a form.","inputSchema":tools[0].input_schema
            }])
        );
        let call = json!({
            "threadId":"thread","turnId":"turn","callId":"call-a","namespace":null,
            "tool":"request_input","arguments":{"purpose":"Survey"}
        });
        assert_eq!(
            serde_json::from_str::<Value>(
                &dynamic_tool_call(&tools, "thread", Some("turn"), &call).unwrap()
            )
            .unwrap(),
            json!({"kind":"tool","name":"request_input","args":{"purpose":"Survey"}})
        );
        assert!(dynamic_tool_call(&tools, "thread", Some("stale"), &call).is_err());
        assert!(dynamic_tool_call(&tools, "different", Some("turn"), &call).is_err());
        let mut unavailable = call;
        unavailable["tool"] = json!("shell");
        assert!(dynamic_tool_call(&tools, "thread", Some("turn"), &unavailable).is_err());
    }
    #[test]
    fn streams_only_the_started_final_answer_item() {
        let mut preview = AnswerItem::default();
        let commentary = json!({"method":"item/started","params":{
            "threadId":"thread","item":{"type":"agentMessage","phase":"commentary","id":"comment","text":"commentary"}
        }});
        assert!(preview.update("thread", &commentary).is_none());
        let final_item = json!({"method":"item/started","params":{
            "threadId":"thread","item":{"type":"agentMessage","phase":"final_answer","id":"answer","text":"part"}
        }});
        assert_eq!(preview.update("thread", &final_item), Some("part"));
        let tool = json!({"method":"item/commandExecution/outputDelta","params":{
            "threadId":"thread","itemId":"answer","delta":"tool output"
        }});
        assert!(preview.update("thread", &tool).is_none());
        let text = json!({"method":"item/agentMessage/delta","params":{
            "threadId":"thread","itemId":"answer","delta":"ial"
        }});
        assert!(preview.update("other-thread", &text).is_none());
        assert_eq!(preview.update("thread", &text), Some("ial"));
        let completed = json!({"method":"item/completed","params":{
            "threadId":"thread","item":{"id":"answer"}
        }});
        assert!(preview.update("thread", &completed).is_none());
        assert!(preview.update("thread", &text).is_none());
    }
}
