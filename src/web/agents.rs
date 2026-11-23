use super::mini::text;
use crate::{
    App,
    forms::FormSpec,
    security::{id, now},
    store::Pending,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

pub(super) async fn save_agent(app: &App, user: &str, data: &Value) -> Result<()> {
    let name = text(data, "name", 80)?;
    let instructions = text(data, "instructions", 2500)?;
    let stored = data["tools"].as_array().context("missing scope")?;
    let allow = [
        "list_mcp_tools",
        "call_mcp",
        "browse",
        "search_web",
        "request_form",
        "request_input",
        "task_files",
        "python",
        "delegate",
        "set_goal",
    ];
    let selected: Vec<_> = stored.iter().filter_map(Value::as_str).collect();
    ensure!(
        selected.len() == stored.len()
            && selected.len() <= allow.len()
            && selected.iter().all(|tool| allow.contains(tool)),
        "unknown agent tool"
    );
    let scope = serde_json::to_string(&selected)?;
    let agent = data["agent"].as_str();
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await?;
    if let Some(pending) = data["pending"].as_str() {
        let form:Pending=sqlx::query_as("SELECT * FROM pending WHERE id=? AND user=? AND state='waiting' AND expires>=? AND kind='agent-spec'")
            .bind(pending).bind(user).bind(now()).fetch_optional(&mut *tx).await?.context("configuration request expired")?;
        let payload: Value = serde_json::from_str(&form.payload)?;
        let schema: FormSpec = serde_json::from_value(json!({"fields":payload["fields"]}))?;
        schema.validate()?;
        let changed=sqlx::query("UPDATE tasks SET state='queued',updated=? WHERE id=? AND revision=? AND state='waiting'")
            .bind(now()).bind(&form.task).bind(form.revision).execute(&mut *tx).await?.rows_affected();
        ensure!(changed == 1, "configuration changed");
        sqlx::query("UPDATE pending SET state='done' WHERE id=?")
            .bind(pending)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO events(task,role,body) VALUES(?,'tool',?)")
            .bind(&form.task)
            .bind(format!("User saved the subagent {name}."))
            .execute(&mut *tx)
            .await?;
    }
    if let Some(agent) = agent {
        let changed =
            sqlx::query("UPDATE agents SET name=?,instructions=?,tools=? WHERE id=? AND owner=?")
                .bind(name)
                .bind(instructions)
                .bind(&scope)
                .bind(agent)
                .bind(user)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        ensure!(changed == 1, "agent unavailable");
    } else {
        sqlx::query("INSERT INTO agents VALUES(?,?,?,?,?)")
            .bind(id())
            .bind(user)
            .bind(name)
            .bind(instructions)
            .bind(scope)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
