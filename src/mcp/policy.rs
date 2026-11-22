use super::{Connector, Mcp};
use anyhow::{Result, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub(super) struct Policy(BTreeMap<String, BTreeSet<String>>);

impl Policy {
    pub(super) fn load() -> Self {
        let Ok(path) = std::env::var("TINYBOT_MCP_READ_POLICY_FILE") else {
            return Self::default();
        };
        match std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        {
            Some(policy) => Self(policy),
            None => {
                tracing::error!("MCP read policy could not be loaded");
                Self::default()
            }
        }
    }
}

impl Mcp {
    pub fn read_tools(&self, endpoint: &str) -> Vec<String> {
        self.policy
            .0
            .get(endpoint)
            .map(|tools| tools.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn verified_read(&self, connector: &Connector, tool: &str) -> bool {
        self.policy
            .0
            .get(&connector.endpoint)
            .is_some_and(|tools| tools.contains(tool))
    }

    pub async fn grant(
        &self,
        user: &str,
        agent: &str,
        connector: &str,
        tool: &str,
        scope: &Value,
    ) -> Result<()> {
        let connector = self.owned(user, connector).await?;
        ensure!(
            self.verified_read(&connector, tool)
                && scope.is_object()
                && scope.to_string().len() <= 4000
                && connector.auth != "pending",
            "the tool is not an operator-verified read"
        );
        let result = sqlx::query(
            "INSERT INTO connector_grants(agent,connector,tool,scope)
            SELECT id,?,?,? FROM agents WHERE id=? AND owner=?
            ON CONFLICT(agent,connector,tool) DO UPDATE SET scope=excluded.scope",
        )
        .bind(&connector.id)
        .bind(tool)
        .bind(scope.to_string())
        .bind(agent)
        .bind(user)
        .execute(&self.db)
        .await?;
        ensure!(result.rows_affected() == 1, "agent unavailable");
        Ok(())
    }

    pub async fn allowed_shared(
        &self,
        agent: &str,
        member: &str,
        connector: &Connector,
        tool: &str,
        args: &Value,
    ) -> Result<bool> {
        if !self.verified_read(connector, tool) {
            return Ok(false);
        }
        let scope: Option<String> = sqlx::query_scalar(
            "SELECT cg.scope FROM connector_grants cg JOIN grants g ON g.agent=cg.agent
            WHERE cg.agent=? AND g.member=? AND cg.connector=? AND cg.tool=?",
        )
        .bind(agent)
        .bind(member)
        .bind(&connector.id)
        .bind(tool)
        .fetch_optional(&self.db)
        .await?;
        Ok(scope
            .and_then(|scope| serde_json::from_str::<Value>(&scope).ok())
            .as_ref()
            == Some(args))
    }
}
