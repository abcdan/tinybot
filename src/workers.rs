use crate::{net, security::clip};
use anyhow::{Context, Result, anyhow, ensure};
use serde_json::{Value, json};
use std::time::Duration;

const MAX_BROWSER_MEDIA_BYTES: usize = 5 * 1024 * 1024;

#[derive(Clone)]
pub struct Workers {
    pub browser: String,
    pub python: String,
}

impl Workers {
    pub(crate) async fn check(&self) -> (Option<WorkerHealth>, Option<WorkerHealth>) {
        tokio::join!(health(&self.browser), health(&self.python))
    }

    pub async fn browser(&self, route: &str, body: &Value) -> Result<Value> {
        call(&self.browser, route, body).await
    }

    pub(crate) async fn browser_capture(
        &self,
        route: &str,
        mime: &str,
        body: &Value,
    ) -> Result<Vec<u8>> {
        let response = request(&self.browser, route, body).await?;
        if !response.status().is_success() {
            let error: Value = serde_json::from_slice(&net::body(response, 4096).await?)?;
            return Err(anyhow!(
                "{}",
                error["error"].as_str().unwrap_or("browser capture failed")
            ));
        }
        ensure!(
            response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                == Some(mime),
            "browser did not return the requested capture format"
        );
        net::body(response, MAX_BROWSER_MEDIA_BYTES).await
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

#[derive(serde::Deserialize, serde::Serialize)]
pub(crate) struct WorkerHealth {
    pub(crate) ok: bool,
    pub(crate) release: Option<String>,
}

async fn health(socket: &str) -> Option<WorkerHealth> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2));
    #[cfg(unix)]
    let client = client.unix_socket(socket);
    #[cfg(not(unix))]
    let _ = socket;
    let response = client
        .build()
        .ok()?
        .get("http://localhost/health")
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let mut health: WorkerHealth =
        serde_json::from_slice(&net::body(response, 4_096).await.ok()?).ok()?;
    health.release = health.release.filter(|value| valid_release(value));
    Some(health)
}

pub(crate) fn valid_release(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) async fn call(socket: &str, route: &str, body: &Value) -> Result<Value> {
    let response = request(socket, route, body).await?;
    let status = response.status();
    let result: Value = serde_json::from_slice(&net::body(response, 900_000).await?)?;
    ensure!(
        status.is_success(),
        "{}",
        result["error"].as_str().unwrap_or("worker request failed")
    );
    Ok(result)
}

async fn request(socket: &str, route: &str, body: &Value) -> Result<reqwest::Response> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(40));
    #[cfg(unix)]
    let client = client.unix_socket(socket);
    #[cfg(not(unix))]
    let _ = socket;
    #[cfg(not(unix))]
    let client = client;
    client
        .build()?
        .post(format!("http://localhost/{route}"))
        .json(body)
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                anyhow!("worker request timed out")
            } else {
                anyhow!("worker unavailable")
            }
        })
}
