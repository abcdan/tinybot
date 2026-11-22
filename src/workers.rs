use crate::{net, security::clip};
use anyhow::{Context, Result, anyhow, ensure};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone)]
pub struct Workers {
    pub browser: String,
    pub python: String,
}

impl Workers {
    pub async fn browser(&self, route: &str, body: &Value) -> Result<Value> {
        call(&self.browser, route, body).await
    }

    pub async fn web_search(&self, query: &str) -> Result<Vec<Value>> {
        let entries = if let Ok(key) = std::env::var("TINYBOT_BRAVE_SEARCH_KEY")
            && !key.is_empty()
        {
            let mut url = net::https("https://api.search.brave.com/res/v1/web/search")?;
            url.query_pairs_mut()
                .append_pair("q", query)
                .append_pair("count", "8")
                .append_pair("result_filter", "web");
            let response = net::request(reqwest::Method::GET, &url)
                .await?
                .header("X-Subscription-Token", key)
                .send()
                .await?
                .error_for_status()?;
            let mut result: Value =
                serde_json::from_slice(&net::body(response, 512 * 1024).await?)?;
            result["web"]["results"].take()
        } else {
            self.browser("search", &json!({"query": query})).await?["results"].take()
        };
        if entries.is_null() {
            return Ok(Vec::new());
        }
        let entries = entries.as_array().context("invalid web search response")?;
        Ok(entries
            .iter()
            .filter_map(|entry| {
                let raw = entry["url"].as_str()?;
                if raw.len() > 2048 {
                    return None;
                }
                let url = net::https(raw).ok()?;
                Some(json!({
                    "title": clip(entry["title"].as_str().unwrap_or(raw), 150),
                    "url": url.as_str(),
                    "description": clip(entry["description"].as_str().unwrap_or(""), 360),
                }))
            })
            .take(8)
            .collect())
    }

    pub async fn python(&self, body: &Value) -> Result<Value> {
        call(&self.python, "run", body).await
    }
}

pub(crate) async fn call(socket: &str, route: &str, body: &Value) -> Result<Value> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(40));
    #[cfg(unix)]
    let client = client.unix_socket(socket);
    #[cfg(not(unix))]
    let _ = socket;
    #[cfg(not(unix))]
    let client = client;
    let response = client
        .build()?
        .post(format!("http://localhost/{route}"))
        .json(body)
        .send()
        .await
        .map_err(|_| anyhow!("worker unavailable"))?;
    let status = response.status();
    let result: Value = serde_json::from_slice(&crate::net::body(response, 900_000).await?)?;
    ensure!(
        status.is_success(),
        "{}",
        result["error"].as_str().unwrap_or("worker request failed")
    );
    Ok(result)
}
