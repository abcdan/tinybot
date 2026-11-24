use super::{McpApprovalDecision, Outbound, Pending, Task, has_access, mcp_approvals};
use crate::security::{clip, now};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use sqlx::SqlitePool;

pub async fn list_requests(db: &SqlitePool, user: &str) -> Result<Vec<Pending>> {
    Ok(sqlx::query_as("SELECT p.* FROM pending p JOIN tasks t ON t.id=p.task
        WHERE p.user=? AND p.state='waiting' AND p.expires>=? AND p.kind IN ('approve','form','form-submit','agent-spec','elicitation','email-send')
        AND t.state='waiting' AND t.revision=p.revision
        AND (t.agent IS NULL OR EXISTS(SELECT 1 FROM agents a WHERE a.id=t.agent
        AND (a.owner=? OR EXISTS(SELECT 1 FROM grants g WHERE g.agent=a.id AND g.member=?))))
        ORDER BY p.expires, p.id LIMIT 30").bind(user).bind(now()).bind(user).bind(user).fetch_all(db).await?)
}

pub async fn pause(
    db: &SqlitePool,
    task: &Task,
    kind: &str,
    payload: &Value,
    notification: &str,
    button: Option<&str>,
    ttl: i64,
    pending_id: &str,
) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let updated = sqlx::query(
        "UPDATE tasks SET state='waiting',updated=? WHERE id=? AND revision=? AND state='running'",
    )
    .bind(now())
    .bind(&task.id)
    .bind(task.revision)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    ensure!(updated == 1, "task changed before request could be created");
    crate::audit::mark_task(&mut tx, task, "waiting").await?;
    sqlx::query(
        "INSERT INTO pending(id,user,task,revision,kind,payload,expires) VALUES(?,?,?,?,?,?,?)",
    )
    .bind(pending_id)
    .bind(&task.user)
    .bind(&task.id)
    .bind(task.revision)
    .bind(kind)
    .bind(payload.to_string())
    .bind(now() + ttl)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO outbox(user,task,agent,body,button) VALUES(?,?,?,?,?)")
        .bind(&task.user)
        .bind(&task.id)
        .bind(&task.agent)
        .bind(notification)
        .bind(button)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn claim_approval(db: &SqlitePool, task: &Task) -> Result<Option<Pending>> {
    Ok(sqlx::query_as("UPDATE pending SET state='executing' WHERE id=(SELECT id FROM pending WHERE task=?
        AND user=? AND revision=? AND state='approved' AND expires>=? ORDER BY expires LIMIT 1) RETURNING *")
        .bind(&task.id).bind(&task.user).bind(task.revision).bind(now()).fetch_optional(db).await?)
}

pub async fn start_action(db: &SqlitePool, task: &Task, action_id: &str) -> Result<bool> {
    let rows = sqlx::query("INSERT INTO pending(id,user,task,revision,kind,payload,state,expires)
        SELECT ?,user,id,revision,'direct','{}','executing',? FROM tasks WHERE id=? AND revision=? AND state='running'")
        .bind(action_id).bind(now()+900).bind(&task.id).bind(task.revision).execute(db).await?.rows_affected();
    Ok(rows == 1)
}

pub async fn finish_action(
    db: &SqlitePool,
    task: &Task,
    action_id: &str,
    result: &str,
    announce: bool,
) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let changed =
        sqlx::query("UPDATE pending SET state='done' WHERE id=? AND task=? AND state='executing'")
            .bind(action_id)
            .bind(&task.id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    ensure!(changed == 1, "action checkpoint missing");
    sqlx::query("INSERT INTO events(task,role,body) VALUES(?,'tool',?)")
        .bind(&task.id)
        .bind(clip(result, 12000))
        .execute(&mut *tx)
        .await?;
    if announce {
        sqlx::query("INSERT INTO outbox(user,task,agent,body) VALUES(?,?,?,?)")
            .bind(&task.user)
            .bind(&task.id)
            .bind(&task.agent)
            .bind(clip(result, 3000))
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn uncertain_action(
    db: &SqlitePool,
    task: &Task,
    action: &str,
    message: &str,
) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query(
        "UPDATE pending SET state='reconciled' WHERE id=? AND task=? AND state='executing'",
    )
    .bind(action)
    .bind(&task.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE tasks SET state='uncertain',updated=? WHERE id=? AND state='running'")
        .bind(now())
        .bind(&task.id)
        .execute(&mut *tx)
        .await?;
    crate::audit::mark_task(&mut tx, task, "uncertain").await?;
    sqlx::query("INSERT INTO events(task,role,body) VALUES(?,'tool',?)")
        .bind(&task.id)
        .bind(message)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO outbox(user,task,agent,body) VALUES(?,?,?,?)")
        .bind(&task.user)
        .bind(&task.id)
        .bind(&task.agent)
        .bind(message)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn respond(db: &SqlitePool, user: &str, pending: &str, approve: bool) -> Result<()> {
    respond_to_request(
        db,
        user,
        pending,
        approval_vote(approve),
        ResponseScope::Any,
    )
    .await
}

pub async fn respond_mcp(
    db: &SqlitePool,
    user: &str,
    pending: &str,
    decision: McpApprovalDecision,
) -> Result<()> {
    respond_to_request(db, user, pending, decision, ResponseScope::Mcp).await
}

pub async fn respond_email(
    db: &SqlitePool,
    user: &str,
    pending: &str,
    approve: bool,
) -> Result<()> {
    respond_to_request(
        db,
        user,
        pending,
        approval_vote(approve),
        ResponseScope::Email,
    )
    .await
}

pub async fn approve_docblock_share(
    db: &SqlitePool,
    user: &str,
    pending: &str,
    days: u64,
) -> Result<()> {
    ensure!(
        crate::mcp::docblock::valid_expiry(days),
        "choose a link expiry from 1 to 365 days"
    );
    respond_to_request(
        db,
        user,
        pending,
        McpApprovalDecision::Once,
        ResponseScope::DocblockShare(days),
    )
    .await
}

enum ResponseScope {
    Any,
    Mcp,
    Email,
    DocblockShare(u64),
}

fn approval_vote(approve: bool) -> McpApprovalDecision {
    if approve {
        McpApprovalDecision::Once
    } else {
        McpApprovalDecision::Deny
    }
}

async fn respond_to_request(
    db: &SqlitePool,
    user: &str,
    pending: &str,
    decision: McpApprovalDecision,
    response_scope: ResponseScope,
) -> Result<()> {
    let approve = decision != McpApprovalDecision::Deny;
    let state = if approve { "approved" } else { "denied" };
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let entry: Pending = sqlx::query_as("UPDATE pending SET state=? WHERE id=? AND user=? AND expires>=?
        AND state='waiting' AND (kind IN ('approve','form-submit','email-send') OR (kind IN ('agent-spec','form','elicitation') AND ?='denied')) AND EXISTS
        (SELECT 1 FROM tasks t WHERE t.id=pending.task AND t.user=pending.user AND t.revision=pending.revision AND t.state='waiting'
        AND (t.agent IS NULL OR EXISTS(SELECT 1 FROM agents a WHERE a.id=t.agent AND
        (a.owner=? OR EXISTS(SELECT 1 FROM grants g WHERE g.agent=a.id AND g.member=?))))) RETURNING *")
        .bind(state).bind(pending).bind(user).bind(now()).bind(state).bind(user).bind(user)
        .fetch_optional(&mut *tx).await?.context("request expired, changed, or already answered")?;
    let scope = if entry.kind == "approve" {
        mcp_approvals::request_scope(&entry.payload)?
    } else {
        None
    };
    match response_scope {
        ResponseScope::Any => {}
        ResponseScope::Mcp => ensure!(scope.is_some(), "MCP approval request unavailable"),
        ResponseScope::Email => ensure!(
            entry.kind == "email-send",
            "Email approval request unavailable"
        ),
        ResponseScope::DocblockShare(days) => {
            let scope = scope.as_ref().context("Docblock share unavailable")?;
            ensure!(
                scope.tool == "create_share"
                    && mcp_approvals::docblock_once(&mut tx, user, scope).await?,
                "Docblock share unavailable"
            );
            let mut payload: Value = serde_json::from_str(&entry.payload)?;
            let arguments = &mut payload["call"]["args"]["arguments"];
            ensure!(
                arguments["kind"] == "note",
                "Docblock note share unavailable"
            );
            arguments["expires_in_days"] = serde_json::json!(days);
            crate::mcp::docblock::review("create_share", arguments)?;
            sqlx::query("UPDATE pending SET payload=? WHERE id=?")
                .bind(payload.to_string())
                .bind(&entry.id)
                .execute(&mut *tx)
                .await?;
        }
    }
    if entry.kind == "email-send" && approve {
        let payload: Value = serde_json::from_str(&entry.payload)?;
        let approval = &payload["smtp_approval"];
        let current: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM mail_accounts a JOIN tasks t ON t.user=a.owner
             WHERE t.id=? AND t.agent IS NULL AND a.owner=? AND a.id=? AND a.epoch=? AND a.can_send=1)",
        ).bind(&entry.task).bind(user).bind(approval["account"].as_str()).bind(approval["epoch"].as_i64())
            .fetch_one(&mut *tx).await?;
        ensure!(
            current
                && approval["sealed"]
                    .as_str()
                    .is_some_and(|sealed| !sealed.is_empty()),
            "SMTP connection changed. Ask for a new draft."
        );
    }
    if let Some(scope) = &scope {
        mcp_approvals::validate_request(&mut tx, user, entry.task.as_deref(), scope).await?;
        if decision == McpApprovalDecision::Always {
            mcp_approvals::remember(&mut tx, user, scope).await?;
        }
    }
    if let (Some(task), Some(revision)) = (&entry.task, entry.revision) {
        crate::audit::request_changed(
            &mut tx,
            task,
            revision,
            &entry.kind,
            match decision {
                McpApprovalDecision::Once => "approved",
                McpApprovalDecision::Always => "approved_always",
                McpApprovalDecision::Deny => "declined",
            },
        )
        .await?;
    }
    if !approve {
        let message = if entry.kind == "email-send" {
            "The user declined this email draft. Do not send it or request approval for it again in this turn."
        } else if scope.is_some() {
            "The user denied this MCP action. Do not request the same action again in this turn."
        } else {
            "User declined the pending action."
        };
        sqlx::query("INSERT INTO events(task,role,body) VALUES(?,'tool',?)")
            .bind(&entry.task)
            .bind(message)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("UPDATE tasks SET state='queued',updated=? WHERE id=?")
        .bind(now())
        .bind(&entry.task)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn notify(db: &SqlitePool, user: &str, body: &str, button: Option<&str>) -> Result<()> {
    sqlx::query("INSERT INTO outbox(user,body,button) VALUES(?,?,?)")
        .bind(user)
        .bind(body)
        .bind(button)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn claim_outbox(db: &SqlitePool) -> Result<Option<Outbound>> {
    Ok(sqlx::query_as(
        "UPDATE outbox SET state='sending' WHERE id=(SELECT id FROM outbox WHERE state='queued'
        AND retry_at<=? ORDER BY id LIMIT 1) RETURNING id,user,agent,task,body,button,widget,reaction_event,reply_to_message_id",
    )
    .bind(now())
    .fetch_optional(db)
    .await?)
}

pub async fn can_deliver(db: &SqlitePool, item: &Outbound) -> Result<bool> {
    if !has_access(db, &item.user, item.agent.as_deref()).await? {
        return Ok(false);
    }
    if let Some(request) = item.button.as_deref().and_then(|button| {
        button
            .strip_prefix("mcp-approval/")
            .or_else(|| button.strip_prefix("mcp-approval-once/"))
    }) {
        return mcp_approvals::waiting(db, item, request).await;
    }
    if let Some(task) = &item.task {
        let usable: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM tasks t WHERE id=? AND user=? AND state<>'revoked'
            AND (t.shared_memory_pair IS NULL OR EXISTS(SELECT 1 FROM available_shared_memory_pairs p
            WHERE p.id=t.shared_memory_pair AND (p.owner=t.user OR p.guest=t.user))))",
        )
        .bind(task)
        .bind(&item.user)
        .fetch_one(db)
        .await?;
        if !usable {
            return Ok(false);
        }
    }
    Ok(true)
}
