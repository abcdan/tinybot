use super::*;
use crate::store::{self, Task};
use serde_json::json;
use sqlx::sqlite::SqlitePoolOptions;

async fn fixture() -> SqlitePool {
    let db = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!().run(&db).await.unwrap();
    for user in ["1", "2"] {
        store::seed(&db, user).await.unwrap();
    }
    sqlx::query(
        "INSERT INTO connectors(id,owner,name,endpoint,auth) VALUES
        ('docs','1','Docs','https://docs.example/mcp','public'),
        ('tickets','1','Tickets','https://tickets.example/mcp','public'),
        ('other','2','Other','https://other.example/mcp','public')",
    )
    .execute(&db)
    .await
    .unwrap();
    db
}

fn scope(connector: &str, tool: &str, epoch: i64) -> McpApprovalScope {
    McpApprovalScope {
        connector: connector.into(),
        tool: tool.into(),
        epoch,
    }
}

fn payload(scope: &McpApprovalScope) -> Value {
    json!({
        "mcp_approval": scope,
        "call": {"name":"call_mcp","args":{"connector":scope.connector,"tool":scope.tool,"arguments":{}}}
    })
}

async fn pause(
    db: &SqlitePool,
    user: &str,
    agent: Option<&str>,
    payload: &Value,
    ttl: i64,
) -> (Task, String) {
    let id = crate::security::id();
    let task: Task = sqlx::query_as(
        "INSERT INTO tasks(id,user,agent,title,state,updated) VALUES(?,?,?,'Read docs','running',?) RETURNING *",
    )
    .bind(id)
    .bind(user)
    .bind(agent)
    .bind(now())
    .fetch_one(db)
    .await
    .unwrap();
    let request = crate::security::id();
    store::pause(
        db,
        &task,
        "approve",
        payload,
        "Review this MCP action",
        Some(&format!("mcp-approval/{request}")),
        ttl,
        &request,
    )
    .await
    .unwrap();
    (task, request)
}

#[tokio::test]
async fn mcp_approval_is_one_use_and_remembering_is_exact_and_revocable() {
    let db = fixture().await;
    let get_page = scope("docs", "get_page", 0);
    let (_, once) = pause(&db, "1", None, &payload(&get_page), 900).await;
    let delivered = store::claim_outbox(&db).await.unwrap().unwrap();
    assert!(store::can_deliver(&db, &delivered).await.unwrap());
    assert!(
        store::respond_mcp(&db, "2", &once, McpApprovalDecision::Always)
            .await
            .is_err()
    );
    store::respond_mcp(&db, "1", &once, McpApprovalDecision::Once)
        .await
        .unwrap();
    assert!(!store::can_deliver(&db, &delivered).await.unwrap());
    assert!(
        store::respond_mcp(&db, "1", &once, McpApprovalDecision::Always)
            .await
            .is_err()
    );
    assert!(!mcp_approved(&db, "1", &get_page).await.unwrap());

    let (running, always) = pause(&db, "1", None, &payload(&get_page), 900).await;
    store::respond_mcp(&db, "1", &always, McpApprovalDecision::Always)
        .await
        .unwrap();
    assert_eq!(
        list_mcp_approvals(&db, "1").await.unwrap(),
        [get_page.clone()]
    );
    assert!(list_mcp_approvals(&db, "2").await.unwrap().is_empty());
    assert!(mcp_approved(&db, "1", &get_page).await.unwrap());
    for other in [
        scope("tickets", "get_page", 0),
        scope("docs", "delete_page", 0),
        scope("docs", "get_page", 1),
    ] {
        assert!(!mcp_approved(&db, "1", &other).await.unwrap());
    }
    assert!(
        !start_remembered_mcp(&db, &running, "remembered", &get_page)
            .await
            .unwrap()
    );
    sqlx::query("UPDATE tasks SET state='running' WHERE id=?")
        .bind(&running.id)
        .execute(&db)
        .await
        .unwrap();
    assert!(
        start_remembered_mcp(&db, &running, "remembered", &get_page)
            .await
            .unwrap()
    );
    let checkpoint: (String, String) =
        sqlx::query_as("SELECT kind,state FROM pending WHERE id='remembered'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(checkpoint, ("approve".into(), "executing".into()));
    assert!(
        !revoke_mcp_approval(&db, "2", "docs", "get_page")
            .await
            .unwrap()
    );
    assert!(
        revoke_mcp_approval(&db, "1", "docs", "get_page")
            .await
            .unwrap()
    );
    assert!(!mcp_approved(&db, "1", &get_page).await.unwrap());
    assert!(
        !start_remembered_mcp(&db, &running, "after_revoke", &get_page)
            .await
            .unwrap()
    );
    assert!(mcp_approval_current(&db, "1", &get_page).await.unwrap());

    let (task, deny) = pause(&db, "1", None, &payload(&get_page), 900).await;
    store::respond_mcp(&db, "1", &deny, McpApprovalDecision::Deny)
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET state='running' WHERE id=?")
        .bind(&task.id)
        .execute(&db)
        .await
        .unwrap();
    assert!(
        mcp_denied_in_turn(&db, &task, &get_page, &json!({}))
            .await
            .unwrap()
    );
    assert!(
        !mcp_denied_in_turn(&db, &task, &get_page, &json!({"page":"next"}))
            .await
            .unwrap()
    );
    assert!(
        !mcp_denied_in_turn(&db, &running, &get_page, &json!({}))
            .await
            .unwrap()
    );
    let history = store::history(&db, &task.id).await.unwrap();
    assert!(
        history
            .last()
            .unwrap()
            .2
            .contains("user denied this MCP action")
    );
}

#[tokio::test]
async fn stale_mcp_approval_buttons_cannot_authorize_changed_connectors_or_tasks() {
    let db = fixture().await;
    let read = scope("docs", "read", 0);
    let (_, grant) = pause(&db, "1", None, &payload(&read), 900).await;
    store::respond_mcp(&db, "1", &grant, McpApprovalDecision::Always)
        .await
        .unwrap();
    let (_, replaced) = pause(&db, "1", None, &payload(&read), 900).await;
    sqlx::query("UPDATE connectors SET endpoint='https://new.example/mcp' WHERE id='docs'")
        .execute(&db)
        .await
        .unwrap();
    let next = scope("docs", "read", 1);
    assert!(mcp_approval_current(&db, "1", &next).await.unwrap());
    assert!(!mcp_approved(&db, "1", &read).await.unwrap());
    assert!(!mcp_approved(&db, "1", &next).await.unwrap());
    assert!(list_mcp_approvals(&db, "1").await.unwrap().is_empty());
    assert!(
        store::respond_mcp(&db, "1", &replaced, McpApprovalDecision::Always)
            .await
            .is_err()
    );
    assert!(store::respond(&db, "1", &replaced, true).await.is_err());

    let (task, old_revision) = pause(&db, "1", None, &payload(&next), 900).await;
    sqlx::query("UPDATE tasks SET revision=revision+1 WHERE id=?")
        .bind(&task.id)
        .execute(&db)
        .await
        .unwrap();
    assert!(
        store::respond_mcp(&db, "1", &old_revision, McpApprovalDecision::Once)
            .await
            .is_err()
    );
    let (_, expired) = pause(&db, "1", None, &payload(&next), -1).await;
    assert!(
        store::respond_mcp(&db, "1", &expired, McpApprovalDecision::Always)
            .await
            .is_err()
    );
    let (_, legacy) = pause(&db, "1", None, &json!({"call":{"name":"call_mcp","args":{"connector":"docs","tool":"read","arguments":{}}}}), 900).await;
    assert!(
        store::respond_mcp(&db, "1", &legacy, McpApprovalDecision::Always)
            .await
            .is_err()
    );
    store::respond(&db, "1", &legacy, true).await.unwrap();
}

#[tokio::test]
async fn mcp_approval_requires_call_binding_and_an_owned_task_scope() {
    let db = fixture().await;
    let own_scope = scope("docs", "get_page", 0);
    let other_scope = scope("other", "get_page", 0);
    let mut mismatched = payload(&own_scope);
    mismatched["call"]["args"]["tool"] = json!("delete_page");
    let (_, request) = pause(&db, "1", None, &mismatched, 900).await;
    assert!(
        store::respond_mcp(&db, "1", &request, McpApprovalDecision::Always)
            .await
            .is_err()
    );
    let (_, request) = pause(&db, "1", None, &payload(&other_scope), 900).await;
    assert!(
        store::respond_mcp(&db, "1", &request, McpApprovalDecision::Once)
            .await
            .is_err()
    );
    sqlx::query("INSERT INTO agents VALUES('guest','1','Guest','Use tools','[]')")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO grants(agent,member) VALUES('guest','2')")
        .execute(&db)
        .await
        .unwrap();
    let (_, request) = pause(&db, "2", Some("guest"), &payload(&other_scope), 900).await;
    assert!(
        store::respond_mcp(&db, "2", &request, McpApprovalDecision::Always)
            .await
            .is_err()
    );
    assert!(list_mcp_approvals(&db, "1").await.unwrap().is_empty());
    assert!(list_mcp_approvals(&db, "2").await.unwrap().is_empty());
}
