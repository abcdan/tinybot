use super::UserKey;
use crate::model::{Models, ServerTool};
use crate::settings::{self, Key};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

impl Models {
    pub(crate) async fn server_tool_settings(&self, user: &str) -> Result<Value> {
        let disabled = self.disabled_server_tools(user).await?;
        Ok(Value::Object(
            ServerTool::ALL
                .into_iter()
                .map(|tool| {
                    (
                        tool.key().to_owned(),
                        json!(!disabled.iter().any(|disabled| disabled == tool.key())),
                    )
                })
                .collect(),
        ))
    }

    pub(crate) async fn set_server_tool(
        &self,
        user: &str,
        tool: &str,
        enabled: bool,
    ) -> Result<()> {
        let key = Key::ALL
            .into_iter()
            .find(|key| {
                key.as_str() == tool && ServerTool::ALL.iter().any(|allowed| allowed.key() == tool)
            })
            .context("unknown OpenRouter server tool")?;
        settings::update_personal(&self.db, user, key, if enabled { "on" } else { "off" }).await
    }

    pub(crate) async fn enabled_server_tools(&self, user: &str) -> Result<Vec<ServerTool>> {
        let active: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM users WHERE id=? AND provider='openrouter' AND model IS NOT NULL
             AND (openrouter_key IS NOT NULL OR credential IS NOT NULL))",
        ).bind(user).fetch_one(&self.db).await?;
        if !active {
            return Ok(Vec::new());
        }
        let disabled = self.disabled_server_tools(user).await?;
        Ok(ServerTool::ALL
            .into_iter()
            .filter(|tool| !disabled.iter().any(|disabled| disabled == tool.key()))
            .collect())
    }

    pub(in crate::model) async fn server_tool_connection(
        &self,
        user: &str,
        tool: ServerTool,
    ) -> Result<(String, UserKey)> {
        let model: Option<String> = sqlx::query_scalar(
            "SELECT model FROM users WHERE id=? AND provider='openrouter' AND model IS NOT NULL
             AND NOT EXISTS(SELECT 1 FROM openrouter_disabled_tools WHERE user=users.id AND tool=?)",
        ).bind(user).bind(tool.key()).fetch_optional(&self.db).await?;
        let model = model
            .context("OpenRouter server tool is disabled or OpenRouter is not the task provider")?;
        ensure!(
            !model.contains('@') && !model.split(':').skip(1).any(|suffix| suffix == "online"),
            "server tools require a model without a preset or online plugin"
        );
        let key = self
            .user_openrouter_key(user)
            .await?
            .context("connect your OpenRouter key first")?;
        Ok((model, key))
    }

    async fn disabled_server_tools(&self, user: &str) -> Result<Vec<String>> {
        Ok(
            sqlx::query_scalar("SELECT tool FROM openrouter_disabled_tools WHERE user=?")
                .bind(user)
                .fetch_all(&self.db)
                .await?,
        )
    }
}
