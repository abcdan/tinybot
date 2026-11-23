use super::{Outbound, Task};
use crate::security::now;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{FromRow, SqliteConnection, SqlitePool};

#[derive(Clone, Debug, Deserialize, Eq, FromRow, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpApprovalScope {
    pub connector: String,
    pub tool: String,
    pub epoch: i64,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum McpApprovalDecision {
    Once,
    Always,
    Deny,
}

pub async fn mcp_approval_current(
    db: &SqlitePool,
    user: &str,
    scope: &McpApprovalScope,
) -> Result<bool> {
    current(&mut *db.acquire().await?, user, scope).await
}

#[cfg(test)]
async fn mcp_approved(db: &SqlitePool, user: &str, scope: &McpApprovalScope) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM mcp_approvals a JOIN connectors c ON c.id=a.connector
         WHERE a.owner=? AND a.connector=? AND a.tool=? AND a.epoch=?
         AND c.owner=a.owner AND c.auth<>'pending' AND c.approval_epoch=a.epoch)",
    )
    .bind(user)
    .bind(&scope.connector)
    .bind(&scope.tool)
    .bind(scope.epoch)
    .fetch_one(db)
    .await?)
}

pub async fn start_remembered_mcp(
    db: &SqlitePool,
    task: &Task,
    action: &str,
    scope: &McpApprovalScope,
) -> Result<bool> {
    // An interrupted remembered action needs the same reconciliation as a one-time approval.
    Ok(sqlx::query(
        "INSERT INTO pending(id,user,task,revision,kind,payload,state,expires)
         SELECT ?,t.user,t.id,t.revision,'approve','{}','executing',?
         FROM tasks t LEFT JOIN agents agent ON agent.id=t.agent
         WHERE t.id=? AND t.user=? AND t.revision=? AND t.agent IS ? AND t.state='running'
         AND (t.agent IS NULL OR agent.owner=t.user)
         AND EXISTS(SELECT 1 FROM mcp_approvals a JOIN connectors c ON c.id=a.connector
            WHERE a.owner=t.user AND a.connector=? AND a.tool=? AND a.epoch=?
            AND c.owner=a.owner AND c.auth<>'pending' AND c.approval_epoch=a.epoch)",
    )
    .bind(action)
    .bind(now() + 900)
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .bind(&task.agent)
    .bind(&scope.connector)
    .bind(&scope.tool)
    .bind(scope.epoch)
    .execute(db)
    .await?
    .rows_affected()
        == 1)
}

pub async fn mcp_denied_in_turn(
    db: &SqlitePool,
    task: &Task,
    scope: &McpApprovalScope,
    arguments: &Value,
) -> Result<bool> {
    let payloads: Vec<String> = sqlx::query_scalar(
        "SELECT p.payload FROM pending p JOIN tasks t ON t.id=p.task AND t.revision=p.revision
         WHERE p.task=? AND p.user=? AND p.revision=? AND p.kind='approve' AND p.state='denied'
         AND t.user=p.user AND t.state='running'",
    )
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .fetch_all(db)
    .await?;
    for raw in payloads {
        if let Ok(payload) = serde_json::from_str::<Value>(&raw)
            && let Ok(Some(denied)) = scope_from_payload(&payload)
            && &denied == scope
            && &payload["call"]["args"]["arguments"] == arguments
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub async fn list_mcp_approvals(db: &SqlitePool, user: &str) -> Result<Vec<McpApprovalScope>> {
    Ok(sqlx::query_as(
        "SELECT a.connector,a.tool,a.epoch FROM mcp_approvals a JOIN connectors c ON c.id=a.connector
         WHERE a.owner=? AND c.owner=a.owner AND c.auth<>'pending' AND c.approval_epoch=a.epoch
         ORDER BY c.name,a.tool",
    )
    .bind(user)
    .fetch_all(db)
    .await?)
}

pub async fn revoke_mcp_approval(
    db: &SqlitePool,
    user: &str,
    connector: &str,
    tool: &str,
) -> Result<bool> {
    Ok(
        sqlx::query("DELETE FROM mcp_approvals WHERE owner=? AND connector=? AND tool=?")
            .bind(user)
            .bind(connector)
            .bind(tool)
            .execute(db)
            .await?
            .rows_affected()
            == 1,
    )
}

pub(super) fn request_scope(payload: &str) -> Result<Option<McpApprovalScope>> {
    // A pre-existing request has no MCP scope and can only use the original once workflow.
    let Ok(payload) = serde_json::from_str::<Value>(payload) else {
        return Ok(None);
    };
    scope_from_payload(&payload)
}

fn scope_from_payload(payload: &Value) -> Result<Option<McpApprovalScope>> {
    let Some(scope) = payload.get("mcp_approval") else {
        return Ok(None);
    };
    let scope: McpApprovalScope = serde_json::from_value(scope.clone())?;
    let call = &payload["call"];
    ensure!(
        !scope.connector.is_empty()
            && scope.connector.len() <= 100
            && !scope.tool.is_empty()
            && scope.tool.len() <= 128
            && scope
                .tool
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
            && scope.epoch >= 0
            && call["name"] == "call_mcp"
            && call["args"]["connector"] == scope.connector
            && call["args"]["tool"] == scope.tool
            && call["args"]["arguments"].is_object(),
        "request is not bound to that MCP tool"
    );
    Ok(Some(scope))
}

pub(super) async fn validate_request(
    connection: &mut SqliteConnection,
    user: &str,
    task: Option<&str>,
    scope: &McpApprovalScope,
) -> Result<()> {
    let owns_task_scope: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM tasks t LEFT JOIN agents a ON a.id=t.agent
         WHERE t.id=? AND t.user=? AND (t.agent IS NULL OR a.owner=?))",
    )
    .bind(task)
    .bind(user)
    .bind(user)
    .fetch_one(&mut *connection)
    .await?;
    ensure!(owns_task_scope, "MCP approval requires an owner task");
    ensure!(
        current(connection, user, scope).await?,
        "MCP connector changed after the request"
    );
    Ok(())
}

pub(super) async fn remember(
    connection: &mut SqliteConnection,
    user: &str,
    scope: &McpApprovalScope,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO mcp_approvals(owner,connector,tool,epoch) VALUES(?,?,?,?)
         ON CONFLICT(owner,connector,tool) DO UPDATE SET epoch=excluded.epoch",
    )
    .bind(user)
    .bind(&scope.connector)
    .bind(&scope.tool)
    .bind(scope.epoch)
    .execute(connection)
    .await?;
    Ok(())
}

pub(super) async fn waiting(db: &SqlitePool, item: &Outbound, request: &str) -> Result<bool> {
    let mut connection = db.acquire().await?;
    let payload: Option<String> = sqlx::query_scalar(
        "SELECT p.payload FROM pending p JOIN tasks t ON t.id=p.task AND t.revision=p.revision
         LEFT JOIN agents a ON a.id=t.agent
         WHERE p.id=? AND p.user=? AND t.user=p.user AND t.id=? AND t.agent IS ?
         AND p.kind='approve' AND p.state='waiting' AND p.expires>=? AND t.state='waiting'
         AND (t.agent IS NULL OR a.owner=t.user)",
    )
    .bind(request)
    .bind(&item.user)
    .bind(&item.task)
    .bind(&item.agent)
    .bind(now())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(scope) = payload
        .as_deref()
        .and_then(|raw| request_scope(raw).ok().flatten())
    else {
        return Ok(false);
    };
    current(&mut connection, &item.user, &scope).await
}

async fn current(
    connection: &mut SqliteConnection,
    user: &str,
    scope: &McpApprovalScope,
) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM connectors WHERE id=? AND owner=? AND approval_epoch=? AND auth<>'pending')",
    )
    .bind(&scope.connector)
    .bind(user)
    .bind(scope.epoch)
    .fetch_one(connection)
    .await?)
}

#[cfg(test)]
mod tests;
