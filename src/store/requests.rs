use super::{
    McpApprovalDecision, Outbound, Pending, Task, has_access, mcp_approvals, settings_requests,
};
use crate::security::{clip, now};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use sqlx::{Executor, Sqlite, SqlitePool, Transaction};

pub async fn list_requests(db: &SqlitePool, user: &str) -> Result<Vec<Pending>> {
    list_requests_in(db, user, None).await
}

pub(super) async fn list_requests_in<'a>(
    db: impl Executor<'a, Database = Sqlite>,
    user: &str,
    task: Option<&str>,
) -> Result<Vec<Pending>> {
    Ok(sqlx::query_as(&format!("SELECT p.* FROM pending p JOIN tasks t ON t.id=p.task AND t.user=p.user
        WHERE p.user=? AND p.state='waiting' AND p.expires>=? AND p.kind IN ('approve','form','form-submit','agent-spec','elicitation','email-send','settings-form','settings-approve')
        AND t.state='waiting' AND t.revision=p.revision AND (? IS NULL OR p.task=?)
        AND EXISTS(SELECT 1 FROM users u WHERE u.id=t.user AND u.suspended_at IS NULL)
        AND (t.shared_memory_pair IS NULL OR EXISTS(SELECT 1 FROM available_shared_memory_pairs sm
          WHERE sm.id=t.shared_memory_pair AND (sm.owner=t.user OR sm.guest=t.user)))
        AND (t.agent IS NULL OR EXISTS(SELECT 1 FROM agents a WHERE a.id=t.agent
        AND (a.owner=? OR EXISTS(SELECT 1 FROM grants g WHERE g.agent=a.id AND g.member=?))))
        AND (p.kind NOT IN ('settings-form','settings-approve') OR (t.user=p.user AND {}))
        ORDER BY p.expires, p.id LIMIT 30", settings_requests::TASK_SCOPE)).bind(user).bind(now()).bind(task).bind(task).bind(user).bind(user).fetch_all(db).await?)
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
    pause_in(
        &mut tx,
        task,
        kind,
        payload,
        notification,
        button,
        ttl,
        pending_id,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub(super) async fn pause_in(
    tx: &mut Transaction<'_, Sqlite>,
    task: &Task,
    kind: &str,
    payload: &Value,
    notification: &str,
    button: Option<&str>,
    ttl: i64,
    pending_id: &str,
) -> Result<()> {
    let updated = sqlx::query(
        "UPDATE tasks SET state='waiting',updated=? WHERE id=? AND revision=? AND state='running'",
    )
    .bind(now())
    .bind(&task.id)
    .bind(task.revision)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    ensure!(updated == 1, "task changed before request could be created");
    crate::audit::mark_task(tx, task, "waiting").await?;
    crate::audit::request_changed(tx, &task.id, task.revision, kind, "waiting").await?;
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
    .execute(&mut **tx)
    .await?;
    sqlx::query("INSERT INTO outbox(user,task,agent,body,button) VALUES(?,?,?,?,?)")
        .bind(&task.user)
        .bind(&task.id)
        .bind(&task.agent)
        .bind(notification)
        .bind(button)
        .execute(&mut **tx)
        .await?;
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

pub(crate) async fn interrupt_action(db: &SqlitePool, task: &Task, action: &str) -> Result<()> {
    sqlx::query(
        "UPDATE pending SET state='interrupted'
         WHERE id=? AND task=? AND user=? AND revision=? AND state='executing'",
    )
    .bind(action)
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn finish_action(
    db: &SqlitePool,
    task: &Task,
    action_id: &str,
    result: &str,
    announce: bool,
) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    finish_action_in(&mut tx, task, action_id, result, announce).await?;
    tx.commit().await?;
    Ok(())
}

pub(super) async fn finish_action_in(
    tx: &mut Transaction<'_, Sqlite>,
    task: &Task,
    action_id: &str,
    result: &str,
    announce: bool,
) -> Result<()> {
    finish_pending_in(tx, task, action_id).await?;
    sqlx::query("INSERT INTO events(task,role,body) VALUES(?,'tool',?)")
        .bind(&task.id)
        .bind(clip(result, 12000))
        .execute(&mut **tx)
        .await?;
    if announce {
        sqlx::query("INSERT INTO outbox(user,task,agent,body) VALUES(?,?,?,?)")
            .bind(&task.user)
            .bind(&task.id)
            .bind(&task.agent)
            .bind(clip(result, 3000))
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

pub(super) async fn finish_pending_in(
    tx: &mut Transaction<'_, Sqlite>,
    task: &Task,
    action_id: &str,
) -> Result<()> {
    let changed =
        sqlx::query("UPDATE pending SET state='done' WHERE id=? AND task=? AND state='executing'")
            .bind(action_id)
            .bind(&task.id)
            .execute(&mut **tx)
            .await?
            .rows_affected();
    ensure!(changed == 1, "action checkpoint missing");
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
    let entry: Pending = sqlx::query_as(&format!("UPDATE pending SET state=? WHERE id=? AND user=? AND expires>=?
        AND state='waiting' AND (kind IN ('approve','form-submit','email-send') OR (kind IN ('agent-spec','form','elicitation','settings-form','settings-approve') AND ?='denied')) AND EXISTS
        (SELECT 1 FROM tasks t WHERE t.id=pending.task AND t.user=pending.user AND t.revision=pending.revision AND t.state='waiting'
        AND (pending.kind NOT IN ('settings-form','settings-approve') OR ({}))
        AND (t.agent IS NULL OR EXISTS(SELECT 1 FROM agents a WHERE a.id=t.agent AND
        (a.owner=? OR EXISTS(SELECT 1 FROM grants g WHERE g.agent=a.id AND g.member=?))))) RETURNING *", settings_requests::TASK_SCOPE))
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
        } else if settings_requests::is_settings_kind(&entry.kind) {
            "The user declined these settings changes. No settings were changed. Do not request them again in this turn."
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
        if entry.kind == "settings-approve"
            && let (Some(task), Some(revision)) = (&entry.task, entry.revision)
            && super::settings_history::origin(&mut tx, task)
                .await?
                .is_some()
        {
            super::settings_history::close_review(&mut tx, task, revision).await?;
            tx.commit().await?;
            return Ok(());
        }
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

pub(crate) async fn claim_delivery(db: &SqlitePool) -> Result<Option<Outbound>> {
    claim_next_outbox(db, true).await
}

/// Tests that inspect produced messages without running the dispatcher can
/// claim independent replies even when their synthetic predecessors remain open.
#[cfg(test)]
pub async fn claim_outbox(db: &SqlitePool) -> Result<Option<Outbound>> {
    claim_next_outbox(db, false).await
}

const NEXT_OUTBOX: &str = "SELECT o.id FROM outbox o WHERE o.state='queued'
    AND o.retry_at<=?1 AND (o.button IS NULL OR substr(o.button,1,?2)<>?3)
    AND (NOT ?4 OR NOT EXISTS(
        SELECT 1 FROM outbox prior WHERE prior.delivery_lane=o.delivery_lane
        AND prior.state IN ('queued','sending')
        AND (prior.state='sending' OR (prior.id<o.id
            AND (prior.button IS NULL OR substr(prior.button,1,?2)<>?3)))))
    AND (o.assistant_event_id IS NULL OR NOT EXISTS(
        SELECT 1 FROM outbox prior WHERE prior.assistant_event_id=o.assistant_event_id
        AND prior.id<o.id AND prior.state IN ('queued','sending')))
    ORDER BY o.id LIMIT 1";

async fn claim_next_outbox(db: &SqlitePool, order_delivery: bool) -> Result<Option<Outbound>> {
    let time = now();
    let wait_prefix = super::acknowledgements::WAIT_PREFIX;
    let wait_prefix_len = wait_prefix.len() as i64;

    // The fallback poll can check an idle or blocked queue without taking SQLite's writer lock.
    let ready: bool = sqlx::query_scalar(&format!("SELECT EXISTS({NEXT_OUTBOX})"))
        .bind(time)
        .bind(wait_prefix_len)
        .bind(wait_prefix)
        .bind(order_delivery)
        .fetch_one(db)
        .await?;
    if !ready {
        return Ok(None);
    }

    // Recheck eligibility during the atomic claim, since another worker may claim the next row.
    Ok(sqlx::query_as(&format!(
        "UPDATE outbox SET state='sending' WHERE id=({NEXT_OUTBOX})
         RETURNING id,user,agent,task,body,button,widget,reaction_event,reply_to_message_id",
    ))
    .bind(time)
    .bind(wait_prefix_len)
    .bind(wait_prefix)
    .bind(order_delivery)
    .fetch_optional(db)
    .await?)
}

pub async fn can_deliver(db: &SqlitePool, item: &Outbound) -> Result<bool> {
    if !has_access(db, &item.user, item.agent.as_deref()).await? {
        return Ok(false);
    }
    if let Some(request) = item
        .button
        .as_deref()
        .and_then(|button| button.strip_prefix("mail-setup/"))
    {
        return Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pending p JOIN tasks t ON t.id=p.task AND t.user=p.user AND t.revision=p.revision WHERE p.id=? AND p.task=? AND p.user=? AND p.kind='mail-setup' AND p.state='waiting' AND p.expires>=? AND t.state='waiting')").bind(request).bind(&item.task).bind(&item.user).bind(now()).fetch_one(db).await?);
    }
    if let Some(request) = item.button.as_deref().and_then(|button| {
        button
            .strip_prefix("mcp-approval/")
            .or_else(|| button.strip_prefix("mcp-approval-once/"))
    }) {
        return mcp_approvals::waiting(db, item, request).await;
    }
    if let Some(request) = item
        .button
        .as_deref()
        .and_then(|button| button.strip_prefix(settings_requests::BUTTON_PREFIX))
    {
        return settings_requests::waiting_delivery(db, item, request).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn blocked_delivery_polls_without_waiting_for_the_writer() {
        let path =
            std::env::temp_dir().join(format!("tinybot-outbox-{}.sqlite", crate::security::id()));
        let db = super::super::open(&path).await.unwrap();
        super::super::seed(&db, "42").await.unwrap();
        super::super::seed(&db, "43").await.unwrap();

        notify(&db, "42", "First reply", None).await.unwrap();
        notify(&db, "42", "Second reply", None).await.unwrap();
        notify(&db, "43", "Future reply", None).await.unwrap();
        sqlx::query("UPDATE outbox SET retry_at=? WHERE user='43'")
            .bind(now() + 60)
            .execute(&db)
            .await
            .unwrap();

        notify(&db, "43", "Later reply in the other chat", None)
            .await
            .unwrap();
        let first = claim_delivery(&db).await.unwrap().unwrap();
        assert_eq!(first.body, "First reply");
        let transaction = db.begin_with("BEGIN IMMEDIATE").await.unwrap();
        let blocked = tokio::time::timeout(Duration::from_secs(1), claim_delivery(&db)).await;
        transaction.rollback().await.unwrap();

        sqlx::query("UPDATE outbox SET state='sent' WHERE id=?")
            .bind(first.id)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE outbox SET retry_at=0 WHERE body='Future reply'")
            .execute(&db)
            .await
            .unwrap();
        let (first_worker, second_worker) = tokio::join!(claim_delivery(&db), claim_delivery(&db));

        db.close().await;
        std::fs::remove_file(&path).unwrap();
        assert!(blocked.unwrap().unwrap().is_none());
        let first_claim = first_worker.unwrap().unwrap();
        let second_claim = second_worker.unwrap().unwrap();
        let mut claimed = [first_claim.body.as_str(), second_claim.body.as_str()];
        claimed.sort_unstable();
        assert_eq!(claimed, ["Future reply", "Second reply"]);
    }
}
