use super::{Mcp, get_json};
use crate::{net, security::clip, workers::Workers};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use url::Url;

const REGISTRY: &str = "https://registry.modelcontextprotocol.io/v0.1/servers";
pub const AUTO: &str = "auto";
pub const HTTP: &str = "streamable-http";
pub const SSE: &str = "sse";
pub const STDIO: &str = "stdio";

#[derive(Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub name: String,
    pub endpoint: String,
    #[serde(default = "auto")]
    pub transport: String,
    #[serde(default)]
    pub publisher: Option<String>,
}

fn auto() -> String {
    AUTO.into()
}

impl Candidate {
    pub fn endpoint(&self) -> Result<String> {
        ensure!(
            matches!(self.transport.as_str(), AUTO | HTTP | SSE | STDIO),
            "unknown MCP transport"
        );
        if self.transport == STDIO {
            let value = Url::parse(&self.endpoint)?;
            ensure!(
                value.scheme() == "stdio"
                    && value.path().is_empty()
                    && value.query().is_none()
                    && value.fragment().is_none()
                    && value.port().is_none()
                    && value.username().is_empty()
                    && value.password().is_none()
                    && value.host_str().is_some_and(|name| !name.is_empty()
                        && name.len() <= 64
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))),
                "use stdio:// followed by an operator-configured program name"
            );
            return Ok(value.to_string());
        }
        Ok(net::https(&self.endpoint)?.to_string())
    }
}

#[derive(Default, Serialize)]
pub struct Discovery {
    pub candidates: Vec<Candidate>,
    pub sources: Vec<Value>,
    pub local_packages: Vec<Value>,
    pub web_search_available: bool,
}

impl Mcp {
    pub async fn discover(&self, workers: &Workers, name: &str) -> Result<Discovery> {
        if let Ok(url) = net::https(name) {
            return Ok(Discovery {
                candidates: vec![Candidate {
                    name: url.host_str().unwrap_or("MCP").into(),
                    endpoint: url.to_string(),
                    transport: AUTO.into(),
                    publisher: None,
                }],
                ..Discovery::default()
            });
        }
        if let Some(alias) = name.strip_prefix("stdio://") {
            let candidate = Candidate {
                name: alias.into(),
                endpoint: name.into(),
                transport: STDIO.into(),
                publisher: None,
            };
            candidate.endpoint()?;
            return Ok(Discovery {
                candidates: vec![candidate],
                ..Discovery::default()
            });
        }
        let query = name
            .split_whitespace()
            .filter(|word| !word.eq_ignore_ascii_case("mcp"))
            .collect::<Vec<_>>()
            .join(" ");
        ensure!(
            !query.is_empty(),
            "enter a product, MCP server name or public endpoint"
        );
        let query = clip(&query, 100);
        let web_query = format!("{query} official MCP server endpoint");
        let (registry, web) = tokio::join!(registry(query), workers.web_search(&web_query));
        if registry.is_err() && web.is_err() {
            anyhow::bail!("the MCP Registry and web search are unavailable");
        }
        let mut discovered = registry.unwrap_or_default();
        if let Ok(links) = web {
            discovered.web_search_available = true;
            for entry in links {
                let Some(raw) = entry["url"].as_str() else {
                    continue;
                };
                let Ok(url) = net::https(raw) else { continue };
                let title = clip(entry["title"].as_str().unwrap_or(raw), 100);
                let suffix = url.path().trim_end_matches('/').rsplit('/').next();
                if matches!(suffix, Some("mcp" | "sse")) && discovered.candidates.len() < 14 {
                    discovered.candidates.push(Candidate {
                        name: title.to_owned(),
                        endpoint: url.to_string(),
                        transport: AUTO.into(),
                        publisher: url.host_str().map(str::to_owned),
                    });
                }
                discovered
                    .sources
                    .push(json!({"title":title,"url":url.as_str(),
                    "description":clip(entry["description"].as_str().unwrap_or(""),360)}));
            }
        }
        let mut seen = BTreeSet::new();
        discovered
            .candidates
            .retain(|row| seen.insert((row.transport.clone(), row.endpoint.clone())));
        Ok(discovered)
    }
}

async fn registry(name: &str) -> Result<Discovery> {
    let mut query = Url::parse(REGISTRY)?;
    query
        .query_pairs_mut()
        .append_pair("search", name)
        .append_pair("limit", "12")
        .append_pair("version", "latest");
    let response = get_json(&query).await?;
    let mut result = Discovery::default();
    for entry in response["servers"].as_array().into_iter().flatten() {
        let server = &entry["server"];
        let publisher = server["name"].as_str().unwrap_or("MCP Registry");
        let name = clip(server["title"].as_str().unwrap_or(publisher), 100);
        for remote in server["remotes"].as_array().into_iter().flatten() {
            let Some(endpoint) = remote["url"].as_str() else {
                continue;
            };
            let Some(transport) = remote["type"]
                .as_str()
                .filter(|value| matches!(*value, HTTP | SSE))
            else {
                continue;
            };
            if endpoint.contains('{') || net::https(endpoint).is_err() {
                continue;
            }
            if result.candidates.len() < 12 {
                result.candidates.push(Candidate {
                    name: name.into(),
                    endpoint: endpoint.into(),
                    transport: transport.into(),
                    publisher: Some(publisher.into()),
                });
            }
        }
        for package in server["packages"].as_array().into_iter().flatten() {
            if package["transport"]["type"] == "stdio" && result.local_packages.len() < 8 {
                result.local_packages.push(json!({"name":name,"publisher":publisher,"identifier":package["identifier"],
                    "registry":package["registryType"],"version":package["version"],"source":server["repository"]["url"]}));
            }
        }
    }
    Ok(result)
}
