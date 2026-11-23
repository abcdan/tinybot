use crate::{App, mcp::Connector, security::clip, store};
use anyhow::Result;
use serde_json::{Value, json};

pub(super) async fn status(app: &App, user: &str) -> Result<Value> {
    let profile: (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT u.provider,u.model,t.id FROM users u LEFT JOIN tasks t ON t.id=u.active_task
         AND t.user=u.id AND t.chief_chat=1 AND t.state NOT IN ('cancelled','revoked')
         AND (t.agent IS NULL OR EXISTS(SELECT 1 FROM agents a WHERE a.id=t.agent
         AND (a.owner=u.id OR EXISTS(SELECT 1 FROM grants g WHERE g.agent=a.id AND g.member=u.id)))) WHERE u.id=?",
    )
    .bind(user)
    .fetch_one(&app.db)
    .await?;
    let agents: Vec<store::Agent> = sqlx::query_as(
        "SELECT * FROM agents a WHERE owner=? OR EXISTS
        (SELECT 1 FROM grants g WHERE g.agent=a.id AND g.member=?) ORDER BY name LIMIT 100",
    )
    .bind(user)
    .bind(user)
    .fetch_all(&app.db)
    .await?;
    let agents = agents
        .into_iter()
        .map(|agent| {
            if agent.owner == user {
                serde_json::to_value(agent).unwrap_or_default()
            } else {
                json!({"id":agent.id,"name":agent.name})
            }
        })
        .collect::<Vec<_>>();
    let tasks = store::list_tasks(&app.db, user).await?;
    let requests = store::list_requests(&app.db, user)
        .await?
        .into_iter()
        .map(|entry| {
            let mut payload: Value = serde_json::from_str(&entry.payload).unwrap_or_default();
            if entry.kind == "approve" {
                let mut details = payload["call"]["args"].clone();
                mask(&mut details);
                payload["details"] = json!(clip(&details.to_string(), 3000));
                payload.as_object_mut().map(|map| map.remove("call"));
            }
            json!({"id":entry.id,"kind":entry.kind,"content":payload,"expires":entry.expires})
        })
        .collect::<Vec<_>>();
    let connectors: Vec<Connector> =
        sqlx::query_as("SELECT * FROM connectors WHERE owner=? ORDER BY name LIMIT 40")
            .bind(user)
            .fetch_all(&app.db)
            .await?;
    let members: Vec<(String, String)> = sqlx::query_as(
        "SELECT g.agent,g.member FROM grants g JOIN agents a ON a.id=g.agent WHERE a.owner=?",
    )
    .bind(user)
    .fetch_all(&app.db)
    .await?;
    let memory: Vec<(String, Option<String>, String)> = sqlx::query_as(
        "SELECT id,agent,body FROM memory WHERE owner=? ORDER BY rowid DESC LIMIT 50",
    )
    .bind(user)
    .fetch_all(&app.db)
    .await?;
    let grants: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT cg.agent,cg.connector,cg.tool,cg.scope FROM connector_grants cg
        JOIN agents a ON a.id=cg.agent WHERE a.owner=?",
    )
    .bind(user)
    .fetch_all(&app.db)
    .await?;
    let schedules = crate::schedules::list(&app.db, user).await?;
    let zone = crate::schedules::timezone(&app.db, user).await?;
    let connections = app.models.connections(user).await?;
    let read_tools: std::collections::BTreeMap<_, _> = connectors
        .iter()
        .map(|connector| (&connector.id, app.mcp.read_tools(&connector.endpoint)))
        .collect();
    Ok(
        json!({"user":user,"provider":profile.0,"model":profile.1,"active_task":profile.2,
        "agents":agents,"tasks":tasks,"requests":requests,"connectors":connectors,"members":members,"memory":memory,"grants":grants,
        "schedules":schedules,"time_zone":zone.name(),"openrouter_connected":connections.openrouter_key,
        "codex_model":connections.codex_model,"read_tools":read_tools}),
    )
}

fn mask(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if [
                    "password",
                    "secret",
                    "token",
                    "credential",
                    "authorization",
                    "api_key",
                    "apikey",
                ]
                .iter()
                .any(|word| key.to_lowercase().contains(word))
                {
                    *value = json!("••••");
                } else {
                    mask(value);
                }
            }
        }
        Value::Array(array) => array.iter_mut().for_each(mask),
        _ => {}
    }
}
