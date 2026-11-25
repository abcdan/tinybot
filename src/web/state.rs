use crate::{App, mcp::Connector, proxy, security::clip, store};
use anyhow::Result;
use serde_json::{Value, json};

pub(super) async fn status(app: &App, user: &str) -> Result<Value> {
    let shared_memory_available: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM shared_memory_pairs WHERE owner=? OR guest=?)",
    )
    .bind(user)
    .bind(user)
    .fetch_one(&app.db)
    .await?;
    let profile: (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT u.provider,u.model,t.id FROM users u LEFT JOIN tasks t ON t.id=u.active_task
         AND t.user=u.id AND t.chief_chat=1 AND t.state NOT IN ('cancelled','completed','revoked')
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
    let tasks = store::task_views::recent(&app.db, user).await?;
    let requests = store::list_requests(&app.db, user)
        .await?
        .iter()
        .filter_map(|entry| request_view(app, user, entry))
        .collect::<Vec<_>>();
    let mail_accounts = store::mail::list_accounts(&app.db, &app.vault, user).await?;
    let connectors: Vec<Connector> =
        sqlx::query_as("SELECT * FROM connectors WHERE owner=? ORDER BY name LIMIT 40")
            .bind(user)
            .fetch_all(&app.db)
            .await?;
    let docblock_access: std::collections::BTreeMap<_, _> = connectors
        .iter()
        .filter_map(|connector| {
            app.mcp
                .docblock_access(connector)
                .map(|access| (&connector.id, access))
        })
        .collect();
    let mcp_approvals: Vec<_> = store::list_mcp_approvals(&app.db, user)
        .await?
        .into_iter()
        .map(|scope| json!({"connector":scope.connector,"tool":scope.tool}))
        .collect();
    let members: Vec<(String, String)> = sqlx::query_as(
        "SELECT g.agent,g.member FROM grants g JOIN agents a ON a.id=g.agent WHERE a.owner=?",
    )
    .bind(user)
    .fetch_all(&app.db)
    .await?;
    let memory: Vec<(String, Option<String>, String)> = sqlx::query_as(
        "SELECT id,agent,body FROM memory WHERE owner=? AND agent IS NOT NULL ORDER BY rowid DESC LIMIT 50",
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
    let backup_model = app.models.backup_setting(user).await?;
    let openrouter_server_tools = app.models.server_tool_settings(user).await?;
    let onboarding = app.models.onboarding_profile(user).await?;
    let browser_proxy = if user == app.operator {
        match proxy::browser_proxy_status() {
            Ok(status) => {
                let source = if status.mode == "proxy" {
                    "saved"
                } else if status.mode == "deployment" && status.deployment_configured {
                    "environment"
                } else {
                    "direct"
                };
                json!({"source":source,"overridden":status.mode!="deployment"})
            }
            Err(_) => json!({"source":"unavailable","overridden":false}),
        }
    } else {
        Value::Null
    };
    let read_tools: std::collections::BTreeMap<_, _> = connectors
        .iter()
        .map(|connector| (&connector.id, app.mcp.read_tools(&connector.endpoint)))
        .collect();
    Ok(
        json!({"user":user,"operator":user==app.operator,"shared_memory_available":shared_memory_available,
        "provider":profile.0,"model":profile.1,"active_task":profile.2,
        "agents":agents,"tasks":tasks,"requests":requests,"connectors":connectors,"docblock_access":docblock_access,"members":members,"memory":memory,"grants":grants,
        "anything_mcp_origin":app.anything_mcp_origin,
        "schedules":schedules,"time_zone":zone.name(),"openrouter_connected":connections.openrouter_key,
        "openrouter_server_tools":openrouter_server_tools,
        "codex_model":connections.codex_model,"backup_model":backup_model,"read_tools":read_tools,"browser_proxy":browser_proxy,
        "onboarding_required":onboarding.required,"display_name":onboarding.display_name,"chief_name":onboarding.chief_name,
        "mcp_approvals":mcp_approvals,"mail_accounts":mail_accounts}),
    )
}

pub(super) fn request_view(app: &App, user: &str, entry: &store::Pending) -> Option<Value> {
    let mut view = if store::settings_requests::is_settings_kind(&entry.kind) {
        store::settings_requests::view(entry).ok()?
    } else {
        let content = if entry.kind == "email-send" {
            let raw: Value = serde_json::from_str(&entry.payload).ok()?;
            let preview = crate::runtime::smtp_preview(&app.vault, user, &entry.id, &raw).ok()?;
            let draft =
                crate::runtime::smtp_editable_fields(&app.vault, user, &entry.id, &raw).ok()?;
            json!({"operation":"Send email","explanation":preview,"draft":draft})
        } else {
            request_content(&entry.kind, &entry.payload)
        };
        json!({"id":entry.id,"kind":entry.kind,"content":content,"expires":entry.expires})
    };
    view["task"] = json!(entry.task);
    view["revision"] = json!(entry.revision);
    Some(view)
}

fn request_content(kind: &str, raw: &str) -> Value {
    let mut payload: Value = serde_json::from_str(raw).unwrap_or_default();
    if kind != "approve" || !payload.is_object() {
        return payload;
    }
    if payload["call"]["name"] == "call_mcp" {
        let operation = payload["operation"].as_str().unwrap_or("MCP call");
        let destination = payload["destination"].as_str();
        if payload["docblock"] == true
            && let Some(tool) = payload["call"]["args"]["tool"].as_str()
            && let Ok(preview) =
                crate::mcp::docblock::review(tool, &payload["call"]["args"]["arguments"])
        {
            return json!({"operation":operation,"destination":destination,"mcp":true,"rememberable":false,"docblock":preview});
        }
        let explanation = payload["explanation"]
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map_or("Run this connector tool to continue the task.", |text| {
                clip(text, 1200)
            });
        return json!({
            "operation": operation,
            "destination": destination,
            "explanation": explanation,
            "mcp": true,
            "rememberable": serde_json::from_value::<store::McpApprovalScope>(payload["mcp_approval"].clone()).is_ok(),
        });
    }
    let mut details = payload["call"]["args"].clone();
    mask(&mut details);
    payload["details"] = json!(clip(&details.to_string(), 3000));
    let _ = payload.as_object_mut().and_then(|map| map.remove("call"));
    payload
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_requests_expose_the_explanation_without_arguments() {
        let payload = json!({
            "operation":"MCP tool search_and_read", "destination":"https://notes.example",
            "call":{"name":"call_mcp","args":{"tool":"search_and_read","arguments":{"query":"fixture-private-value"}}},
            "explanation":"Search project notes for the latest release.",
            "mcp_approval":{"connector":"notes","tool":"search_and_read","epoch":0}
        });
        let projected = request_content("approve", &payload.to_string());
        assert_eq!(
            projected["explanation"],
            "Search project notes for the latest release."
        );
        assert_eq!(projected["mcp"], true);
        assert_eq!(projected["rememberable"], true);
        assert!(projected["call"].is_null() && projected["details"].is_null());
        assert!(!projected.to_string().contains("fixture-private-value"));

        let mut legacy = payload;
        legacy.as_object_mut().unwrap().remove("explanation");
        legacy.as_object_mut().unwrap().remove("mcp_approval");
        let projected = request_content("approve", &legacy.to_string());
        assert_eq!(
            projected["explanation"],
            "Run this connector tool to continue the task."
        );
        assert_eq!(projected["rememberable"], false);
        assert!(!projected.to_string().contains("fixture-private-value"));
    }
}
