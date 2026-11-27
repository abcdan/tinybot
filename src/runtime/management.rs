use super::{Intent, Outcome, Risk, Tool, ToolContext, settings::available};
use crate::{store, web};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};

pub(super) struct ManageAccount;

// Only operations whose inputs and results can enter a private Chief conversation.
const OPERATIONS: &[&str] = &[
    "list-connections",
    "connect-mail",
    "connection-policy",
    "folder-rule",
    "pause-folder",
    "resume-folder",
    "receipt-rule",
    "create-routine",
    "list-folders",
    "docblock-workspaces",
    "docblock-folders",
    "save-agent",
    "restore-settings",
    "save-browser-proxy",
    "remove-browser-proxy",
    "disable-browser-proxy",
    "list-projects",
    "create-project",
    "project-detail",
    "project-member",
    "project-item",
    "preview-project-result",
    "publish-project-item",
    "list-memories",
    "delete-memory",
    "publish-memory",
    "shared-memory-pairs",
    "join-shared-memory",
    "leave-shared-memory",
    "shared-memory-notes",
    "revoke-docblock-share",
    "publish-shared-memory",
    "remove-shared-memory",
    "grant-connector",
    "revoke-connector",
    "revoke-member",
    "list-invites",
    "create-invite",
    "invite-agent",
    "revoke-invite",
    "suspend-guest",
    "restore-guest",
    "invite-shared-memory",
    "calendar-settings",
    "set-calendar",
    "remove-calendar",
    "disconnect-mcp",
    "remove-mail-account",
    "disconnect-openrouter",
    "disconnect-codex",
    "settings-history",
    "save-usage-alert",
    "pause-schedule",
    "resume-schedule",
    "schedule-details",
    "schedule-policy",
    "edit-schedule",
    "cancel-schedule",
    "schedule-history",
    "run-schedule",
    "snooze-schedule",
];

fn is_read(action: &str) -> bool {
    matches!(
        action,
        "list-connections"
            | "connect-mail"
            | "list-folders"
            | "list-projects"
            | "project-detail"
            | "preview-project-result"
            | "list-memories"
            | "shared-memory-pairs"
            | "list-invites"
            | "calendar-settings"
            | "settings-history"
            | "schedule-details"
            | "schedule-history"
            | "docblock-workspaces"
            | "docblock-folders"
    )
}

/// An interrupted management write must be reconciled just like a connected write.
pub(super) async fn reserve(ctx: &ToolContext<'_>, args: &Value, id: &str) -> Result<bool> {
    let action = args["action"].as_str().context("Missing operation")?;
    if is_read(action) {
        return store::start_action(&ctx.app.db, ctx.task, id).await;
    }
    let fingerprint = crate::security::hash(&args.to_string());
    let inserted=sqlx::query("INSERT INTO pending(id,user,task,revision,kind,payload,state,expires)
        SELECT ?,t.user,t.id,t.revision,'approve',json_object('management_fingerprint',?),'executing',?
        FROM tasks t WHERE t.id=? AND t.user=? AND t.revision=? AND t.state='running'
        AND NOT EXISTS(SELECT 1 FROM pending p WHERE p.task=t.id AND p.revision=t.revision
            AND json_extract(p.payload,'$.management_fingerprint')=?)")
        .bind(id).bind(&fingerprint).bind(crate::security::now()+900).bind(&ctx.task.id)
        .bind(&ctx.task.user).bind(ctx.task.revision).bind(&fingerprint).execute(&ctx.app.db).await?.rows_affected();
    Ok(inserted == 1)
}

#[async_trait]
impl Tool for ManageAccount {
    fn name(&self) -> &'static str {
        "manage_account"
    }
    fn describe(&self) -> &'static str {
        "Manage this owner's account on their explicit instruction. List connections for IDs and current action policies. connection-policy takes id (or all) and automatic boolean. For a missing inbox or sender use connect-mail with capability imap or smtp. folder-rule takes id, folder name, instruction, automate, optional remove. receipt-rule takes mail account id, folder, connected Docblock connector id in connector, workspace, and failures_only boolean. list-folders takes no arguments. For schedules use id and revision from schedule-details. save-agent takes name, instructions, tools, optional agent ID. Use current IDs, ask when ambiguous. Never supply credentials."
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{"action":{"enum":OPERATIONS},"data":{"type":"object"}},"required":["action","data"],"additionalProperties":false})
    }
    async fn inspect(&self, ctx: &ToolContext<'_>, args: &Value) -> Result<Intent> {
        ensure!(
            available(ctx).await?
                && OPERATIONS.contains(&args["action"].as_str().unwrap_or(""))
                && args["data"].is_object(),
            "Unavailable account operation."
        );
        Ok(Intent {
            risk: Risk::Local,
            destination: "Your TinyBot account".into(),
            detail: "Manage account".into(),
        })
    }
    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> Result<Outcome> {
        self.inspect(ctx, &args).await?;
        let app = ctx.app;
        let user = &ctx.task.user;
        let data = &args["data"];
        let action = args["action"].as_str().context("Missing operation")?;
        if !is_read(action) {
            ensure!(
                super::automatic::personal_instruction(ctx, action, data).await?,
                "Ask for the exact account change in your private chat."
            );
        }
        if action == "save-browser-proxy" {
            let url = url::Url::parse(data["url"].as_str().context("Choose a public proxy URL")?)?;
            ensure!(
                url.username().is_empty() && url.password().is_none(),
                "Use the private browser settings screen for proxy credentials."
            );
        }
        let result = match action {
            "restore-settings" => {
                store::settings_requests::restore_direct(
                    app,
                    ctx.task,
                    data["id"]
                        .as_i64()
                        .context("Choose a settings history entry")?,
                )
                .await?
            }
            "connect-mail" => {
                let capability = data["capability"]
                    .as_str()
                    .filter(|c| matches!(*c, "smtp" | "imap"))
                    .context("Choose imap or smtp")?;
                let request = crate::security::id();
                store::pause(&app.db,ctx.task,"mail-setup",&json!({"capability":capability}),"Connect your email account in the private setup screen. I’ll resume this task when you save it.",Some(&format!("mail-setup/{request}")),1800,&request).await?;
                return Ok(Outcome::Pause);
            }
            "list-connections" => {
                let rows:Vec<(String,String,String,bool)>=sqlx::query_as("SELECT id,name,'connector',automatic_actions FROM connectors WHERE owner=? UNION ALL SELECT id,label,'mail',automatic_actions FROM mail_accounts WHERE owner=? ORDER BY 2").bind(user).bind(user).fetch_all(&app.db).await?;
                json!(rows.into_iter().map(|(id,name,kind,automatic)|json!({ "id":id,"name":name,"kind":kind,"automatic":automatic})).collect::<Vec<_>>())
            }
            "connection-policy" => {
                let id = data["id"].as_str().context("Choose a connection or all")?;
                let automatic = data["automatic"]
                    .as_bool()
                    .context("Choose automatic or review")?;
                let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await?;
                let mut count = 0;
                for table in ["connectors", "mail_accounts"] {
                    count += sqlx::query(&format!(
                        "UPDATE {table} SET automatic_actions=? WHERE owner=? AND (id=? OR ?='all')"
                    ))
                    .bind(automatic)
                    .bind(user)
                    .bind(id)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                }
                ensure!(count > 0, "No matching connection");
                if !automatic {
                    sqlx::query(
                        "DELETE FROM mcp_approvals WHERE owner=? AND (connector=? OR ?='all')",
                    )
                    .bind(user)
                    .bind(id)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                }
                tx.commit().await?;
                json!({"updated":count,"automatic":automatic})
            }
            "pause-folder" | "resume-folder" => {
                store::mail::pause_folder(
                    app,
                    user,
                    data["id"].as_str().context("Choose an account")?,
                    data["folder"].as_str().context("Choose a folder")?,
                    action == "pause-folder",
                )
                .await?;
                json!({"paused":action=="pause-folder"})
            }
            "folder-rule" | "receipt-rule" => {
                let receipt = if action == "receipt-rule" {
                    let connector = app
                        .mcp
                        .owned(
                            user,
                            data["connector"]
                                .as_str()
                                .context("Choose a Docblock connection")?,
                        )
                        .await?;
                    ensure!(
                        crate::mcp::docblock::is_endpoint(&connector.endpoint)
                            && app.mcp.docblock_access(&connector) == Some("write"),
                        "Connect Docblock with write access."
                    );
                    let workspace = data["workspace"]
                        .as_str()
                        .filter(|s| !s.trim().is_empty() && s.len() <= 160)
                        .context("Choose a workspace")?;
                    let granted = app.mcp.docblock_workspaces(&connector).await?;
                    ensure!(
                        granted["workspaces"]
                            .as_array()
                            .is_some_and(|list| list.iter().any(|entry| entry["id"] == workspace
                                && matches!(entry["role"].as_str(), Some("owner" | "editor")))),
                        "Choose a granted writable workspace."
                    );
                    Some((
                        connector.id,
                        connector.approval_epoch,
                        workspace,
                        data["failures_only"] == true,
                    ))
                } else {
                    None
                };
                let folder = store::mail::FolderConfig {
                    name: data["folder"].as_str().context("Choose a folder")?.into(),
                    automate: receipt.is_some() || data["automate"].as_bool().unwrap_or(false),
                    instruction: receipt.as_ref().map(|(_,_,workspace,_)| format!("Extract merchant, date and total from this receipt and file it in Docblock workspace {workspace}.")).or_else(||data["instruction"].as_str().map(str::to_owned)),
                };
                store::mail::configure_folder(
                    app,
                    user,
                    data["id"].as_str().context("Choose an account")?,
                    folder,
                    data["remove"] == true,
                    receipt
                        .as_ref()
                        .map(|(id, epoch, w, q)| (id.as_str(), *epoch, *w, *q)),
                )
                .await?;
                json!({"saved":true})
            }
            "list-folders" => {
                let accounts = serde_json::to_value(
                    store::mail::list_accounts(&app.db, &app.vault, user).await?,
                )?;
                let rules:Vec<(String,String,String,String,bool)>=sqlx::query_as("SELECT r.account,r.folder,c.name,r.workspace,r.failures_only FROM receipt_rules r JOIN connectors c ON c.id=r.connector JOIN mail_accounts a ON a.id=r.account WHERE c.owner=? AND a.owner=?").bind(user).bind(user).fetch_all(&app.db).await?;
                json!({"accounts":accounts.as_array().into_iter().flatten().map(|a|json!({"id":a["id"],"label":a["label"],"folders":a["folders"]})).collect::<Vec<_>>(),"receipt_rules":rules.into_iter().map(|(account,folder,connection,workspace,failures_only)|json!({ "account":account,"folder":folder,"connection":connection,"workspace":workspace,"failures_only":failures_only})).collect::<Vec<_>>()})
            }
            "invite-agent" | "create-invite" => {
                let invitation = web::chief_operation(app, user, action, data).await?;
                let command = invitation["command"]
                    .as_str()
                    .context("Invitation unavailable")?;
                store::notify(
                    &app.db,
                    user,
                    &format!("Share this one-time invitation: {command}"),
                    None,
                )
                .await?;
                json!({"invitation_sent_to_your_chat":true})
            }
            _ => web::chief_operation(app, user, action, data).await?,
        };
        Ok(Outcome::Continue(result))
    }
}
