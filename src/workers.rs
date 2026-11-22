use anyhow::{Result, anyhow, ensure};
use serde_json::Value;
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
