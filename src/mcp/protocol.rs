use super::{
    LIMIT,
    discovery::{AUTO, SSE},
};
use crate::{net, security::clip};
use anyhow::{Context, Result, anyhow, ensure};
use reqwest::{Method, StatusCode, header::HeaderMap};
use serde_json::{Value, json};
use url::Url;

const CLASSIC: &str = "2025-11-25";
const MODERN: &str = "2026-07-28";

pub(super) fn initialize(version: &str) -> Value {
    json!({"jsonrpc":"2.0","method":"initialize","id":1,"params":{
        "protocolVersion":version,"capabilities":{},"clientInfo":identity()
    }})
}

fn identity() -> Value {
    json!({"name":"tinybot","version":env!("CARGO_PKG_VERSION")})
}

fn discovery() -> Value {
    modern_message(1, "server/discover", json!({}))
}

fn modern_message(id: u64, method: &str, mut params: Value) -> Value {
    params["_meta"] = json!({"io.modelcontextprotocol/clientInfo":identity()});
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}

enum Transport {
    Modern,
    Classic {
        session: Option<String>,
        version: String,
    },
    Legacy {
        messages: Url,
        stream: Sse,
    },
}

pub(super) struct Session {
    url: Url,
    auth: HeaderMap,
    transport: Transport,
    next: u64,
}

pub(super) async fn probe(url: &Url, transport: &str) -> Result<reqwest::Response> {
    if transport == SSE {
        return listen(url, &HeaderMap::new()).await;
    }
    let headers = HeaderMap::new();
    let modern = post(url, &headers, &discovery(), MODERN, None).await?;
    if matches!(
        modern.status(),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ) {
        return Ok(modern);
    }
    let classic = post(url, &headers, &initialize(CLASSIC), CLASSIC, None).await?;
    if transport == AUTO
        && matches!(
            classic.status(),
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
        )
    {
        return listen(url, &headers).await;
    }
    Ok(classic)
}

impl Session {
    pub(super) async fn open(url: &str, transport: &str, auth: HeaderMap) -> Result<Self> {
        let url = net::https(url)?;
        let choice = if transport == SSE {
            legacy(&url, &auth).await?
        } else {
            let modern = post(&url, &auth, &discovery(), MODERN, None).await?;
            if modern.status().is_success() && response_value(modern, 1).await.is_ok() {
                Transport::Modern
            } else {
                let classic = post(&url, &auth, &initialize(CLASSIC), CLASSIC, None).await?;
                if transport == AUTO
                    && matches!(
                        classic.status(),
                        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
                    )
                {
                    legacy(&url, &auth).await?
                } else {
                    let session = classic
                        .headers()
                        .get("mcp-session-id")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    let version = protocol_version(response_value(classic, 1).await?)?;
                    let ack =
                        post(&url, &auth, &initialized(), &version, session.as_deref()).await?;
                    ensure!(
                        ack.status().is_success(),
                        "MCP session initialization failed"
                    );
                    Transport::Classic { session, version }
                }
            }
        };
        Ok(Self {
            url,
            auth,
            transport: choice,
            next: 2,
        })
    }

    pub(super) async fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next;
        self.next += 1;
        match &mut self.transport {
            Transport::Modern => {
                let message = modern_message(id, method, params);
                response_value(
                    post(&self.url, &self.auth, &message, MODERN, None).await?,
                    id,
                )
                .await
            }
            Transport::Classic { session, version } => {
                let message = json!({"jsonrpc":"2.0","method":method,"id":id,"params":params});
                response_value(
                    post(&self.url, &self.auth, &message, version, session.as_deref()).await?,
                    id,
                )
                .await
            }
            Transport::Legacy { messages, stream } => {
                let message = json!({"jsonrpc":"2.0","method":method,"id":id,"params":params});
                legacy_post(messages, &self.auth, &message).await?;
                stream.response(id).await
            }
        }
    }

    pub(super) async fn tools(&mut self) -> Result<Value> {
        let mut result = Vec::new();
        let mut params = json!({});
        for _ in 0..4 {
            let page = self.rpc("tools/list", params).await?;
            let tools = page["tools"].as_array().context("invalid MCP tool list")?;
            result.extend(tools.iter().take(250-result.len()).map(|tool|json!({
                "name":tool["name"],"description":clip(tool["description"].as_str().unwrap_or(""),350),
                "inputSchema":tool["inputSchema"]
            })));
            let Some(cursor) = page["nextCursor"].as_str().filter(|_| result.len() < 250) else {
                break;
            };
            params = json!({"cursor":cursor});
        }
        Ok(Value::Array(result))
    }
}

fn initialized() -> Value {
    json!({"jsonrpc":"2.0","method":"notifications/initialized"})
}

fn protocol_version(info: Value) -> Result<String> {
    let version = info["protocolVersion"]
        .as_str()
        .context("invalid MCP handshake")?;
    ensure!(
        matches!(
            version,
            "2025-11-25" | "2025-06-18" | "2025-03-26" | "2024-11-05"
        ),
        "unsupported MCP version"
    );
    Ok(version.into())
}

async fn post(
    url: &Url,
    auth: &HeaderMap,
    message: &Value,
    version: &str,
    session: Option<&str>,
) -> Result<reqwest::Response> {
    let mut request = net::request(Method::POST, url)
        .await?
        .headers(auth.clone())
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", version)
        .json(message);
    if version == MODERN {
        request = request.header(
            "mcp-method",
            message["method"].as_str().context("missing MCP method")?,
        );
        if let Some(name) = message["params"]["name"].as_str() {
            request = request.header("mcp-name", name);
        }
    }
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }
    Ok(request.send().await?)
}

async fn listen(url: &Url, auth: &HeaderMap) -> Result<reqwest::Response> {
    Ok(net::request(Method::GET, url)
        .await?
        .headers(auth.clone())
        .header("accept", "text/event-stream")
        .send()
        .await?)
}

async fn legacy(url: &Url, auth: &HeaderMap) -> Result<Transport> {
    let mut stream = Sse::new(listen(url, auth).await?)?;
    let messages = loop {
        let (event, data) = stream.next().await?;
        if event == "endpoint" {
            let endpoint = url.join(data.trim())?;
            ensure!(
                endpoint.origin() == url.origin(),
                "legacy MCP message endpoint changed origin"
            );
            break endpoint;
        }
    };
    legacy_post(&messages, auth, &initialize("2024-11-05")).await?;
    protocol_version(stream.response(1).await?)?;
    legacy_post(&messages, auth, &initialized()).await?;
    Ok(Transport::Legacy { messages, stream })
}

async fn legacy_post(url: &Url, auth: &HeaderMap, message: &Value) -> Result<()> {
    let status = net::request(Method::POST, url)
        .await?
        .headers(auth.clone())
        .json(message)
        .send()
        .await?
        .status();
    ensure!(status.is_success(), "legacy MCP message was rejected");
    Ok(())
}

fn event_stream(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get("content-type")
        .and_then(|header| header.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"))
}

async fn response_value(response: reqwest::Response, id: u64) -> Result<Value> {
    ensure!(
        response.status().is_success(),
        "MCP request failed (HTTP {})",
        response.status().as_u16()
    );
    if event_stream(&response) {
        Sse::new(response)?.response(id).await
    } else {
        checked_result(
            serde_json::from_slice(&net::body(response, LIMIT).await?)?,
            id,
        )
    }
}

fn checked_result(mut value: Value, id: u64) -> Result<Value> {
    ensure!(
        value["id"] == id && value.get("error").is_none(),
        "MCP server rejected the request"
    );
    ensure!(value.get("result").is_some(), "MCP response has no result");
    Ok(value["result"].take())
}

struct Sse {
    response: reqwest::Response,
    buffer: Vec<u8>,
    total: usize,
}

impl Sse {
    fn new(response: reqwest::Response) -> Result<Self> {
        ensure!(
            response.status().is_success() && event_stream(&response),
            "MCP event stream unavailable"
        );
        Ok(Self {
            response,
            buffer: Vec::new(),
            total: 0,
        })
    }

    async fn next(&mut self) -> Result<(String, String)> {
        let mut event = String::new();
        let mut data = String::new();
        loop {
            while let Some(index) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<_> = self.buffer.drain(..=index).collect();
                let line = std::str::from_utf8(&line)?.trim_end_matches(['\r', '\n']);
                if line.is_empty() {
                    if !data.is_empty() {
                        return Ok((event, data));
                    }
                    event.clear();
                } else if let Some(value) = line.strip_prefix("event:") {
                    event = value.trim().into();
                } else if let Some(value) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(value.strip_prefix(' ').unwrap_or(value));
                }
            }
            let chunk = self
                .response
                .chunk()
                .await?
                .ok_or_else(|| anyhow!("MCP stream ended before the response"))?;
            self.total += chunk.len();
            ensure!(self.total <= LIMIT, "MCP stream exceeds size limit");
            self.buffer.extend_from_slice(&chunk);
        }
    }

    async fn response(&mut self, id: u64) -> Result<Value> {
        loop {
            let (_, data) = self.next().await?;
            let event: Value = serde_json::from_str(&data)?;
            if event["id"] == id {
                return checked_result(event, id);
            }
        }
    }
}
