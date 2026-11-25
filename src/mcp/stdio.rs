use super::{ApiCredential, Connector, Mcp, binding};
use crate::workers;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use url::Url;

impl Mcp {
    pub(super) async fn stdio_request(
        &self,
        connector: &Connector,
        key: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let alias = Url::parse(&connector.endpoint)?
            .host_str()
            .context("missing stdio program")?
            .to_owned();
        let socket = std::env::var("TINYBOT_MCP_STDIO_SOCKET")
            .unwrap_or_else(|_| "/mcp-stdio/stdio.sock".into());
        #[cfg(test)]
        let socket = self.stdio_fixture.as_deref().unwrap_or(&socket);
        workers::call(
            socket.as_ref(),
            "rpc",
            &json!({"alias":alias,"credential":key,"method":method,"params":params}),
        )
        .await
        .context("configured stdio worker failed; check the program and Compose stdio profile")
    }

    pub(super) async fn stdio(
        &self,
        connector: &Connector,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let connector = self.current_connection(connector).await?;
        let key = match connector.auth.as_str() {
            "public" => None,
            "api" => {
                let secret = self.vault.open(
                    &binding(&connector),
                    connector
                        .credential
                        .as_deref()
                        .context("missing stdio credential")?,
                )?;
                Some(ApiCredential::decode(secret)?.key)
            }
            _ => anyhow::bail!("stdio connector needs setup"),
        };
        ensure!(
            key.as_ref()
                .is_none_or(|key| !key.is_empty() && key.len() <= 8192),
            "invalid stdio credential"
        );
        self.stdio_request(&connector, key.as_deref(), method, params)
            .await
    }
}
