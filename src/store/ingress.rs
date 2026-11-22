use crate::security::{clip, hash, id, now, token};
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn ingest(
    db: &SqlitePool,
    operator: &str,
    channel: &str,
    external_id: i64,
    user: &str,
    text: &str,
) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let inserted = sqlx::query("INSERT OR IGNORE INTO updates VALUES (?,?)")
        .bind(channel)
        .bind(external_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if inserted == 0 {
        return Ok(());
    }
    let text = clip(text.trim(), 8000);
    let (command, argument) = text.split_once(' ').unwrap_or((text, ""));
    let command = command.split('@').next().unwrap_or(command);
    let argument = argument.trim();
    let mut response = None;
    if command == "/start" && !argument.is_empty() {
        let invite: Option<(Option<String>,)> =
            sqlx::query_as("DELETE FROM invitations WHERE token=? AND expires>=? RETURNING agent")
                .bind(hash(argument))
                .bind(now())
                .fetch_optional(&mut *tx)
                .await?;
        if let Some((agent,)) = invite {
            sqlx::query("INSERT OR IGNORE INTO users(id,host_id,onboarded_at) VALUES(?,?,?)")
                .bind(user)
                .bind(format!("urn:uuid:{}", id()))
                .bind(now())
                .execute(&mut *tx)
                .await?;
            if let Some(agent) = agent {
                sqlx::query("INSERT OR IGNORE INTO grants VALUES(?,?)")
                    .bind(&agent)
                    .bind(user)
                    .execute(&mut *tx)
                    .await?;
                response = Some(format!(
                    "Invitation accepted. Open Settings & forms or use /use {agent} to ask this agent a question."
                ));
            }
        } else {
            response = Some("This invitation is invalid or expired.".into());
        }
    }
    let profile: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT active_task,provider FROM users WHERE id=?")
            .bind(user)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((active_task, provider)) = profile else {
        tx.commit().await?;
        return Ok(());
    };
    let mut active = active_task;
    if response.is_none() {
        match command {
            "/start" | "/help" => response = Some("Use the panel for requests, tasks, and schedules. Set up your model under Settings & forms. Use /new to start a task, /panel to reopen the panel, /cancel to stop, or /reconcile after checking an uncertain action.".into()),
            "/panel" => response = Some("Manage TinyBot from this chat.".into()),
            "/invite" if user == operator => {
                let raw = token();
                sqlx::query("INSERT INTO invitations VALUES (?,?,NULL,?)").bind(hash(&raw)).bind(user)
                    .bind(now()+86400).execute(&mut *tx).await?;
                response = Some(format!("Have your guest send /start {raw} to this bot. This invitation expires in 24 hours."));
            }
            "/tasks" => {
                let rows: Vec<(String,String,String)> = sqlx::query_as("SELECT id,title,state FROM tasks
                    WHERE user=? AND state<>'revoked' AND (agent IS NULL OR EXISTS(SELECT 1 FROM agents a WHERE a.id=tasks.agent
                    AND (a.owner=? OR EXISTS(SELECT 1 FROM grants g WHERE g.agent=a.id AND g.member=?))))
                    ORDER BY updated DESC LIMIT 12").bind(user).bind(user).bind(user).fetch_all(&mut *tx).await?;
                response = Some(if rows.is_empty() { "You have no tasks yet.".into() } else {
                    rows.into_iter().map(|(id,title,state)| format!("{title} ({state})\n/switch {id}")).collect::<Vec<_>>().join("\n\n")
                });
            }
            "/switch" => {
                let found: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tasks t WHERE t.id=? AND t.user=? AND t.state NOT IN ('cancelled','revoked')
                    AND (agent IS NULL OR EXISTS(SELECT 1 FROM agents a WHERE a.id=t.agent AND
                    (a.owner=? OR EXISTS(SELECT 1 FROM grants g WHERE g.agent=a.id AND g.member=?)))))")
                    .bind(argument).bind(user).bind(user).bind(user).fetch_one(&mut *tx).await?;
                if found {
                    sqlx::query("UPDATE users SET active_task=? WHERE id=?").bind(argument).bind(user).execute(&mut *tx).await?;
                }
                response = Some(if found {"Switched tasks."} else {"Task unavailable."}.into());
            }
            "/cancel" => {
                if let Some(task) = active.as_deref() {
                    sqlx::query("UPDATE tasks SET state='cancelled',revision=revision+1 WHERE id=? AND user=? AND state<>'revoked'")
                        .bind(task).bind(user).execute(&mut *tx).await?;
                    sqlx::query("UPDATE pending SET state='superseded' WHERE task=? AND state IN ('waiting','approved')")
                        .bind(task).execute(&mut *tx).await?;
                    sqlx::query("UPDATE users SET active_task=NULL WHERE id=?").bind(user).execute(&mut *tx).await?;
                }
                response = Some("Active task stopped. An action already in flight may still report its result.".into());
            }
            "/new" if argument.is_empty() => {
                sqlx::query("UPDATE users SET active_task=NULL WHERE id=?").bind(user).execute(&mut *tx).await?;
                response = Some("Send a message to start your new task.".into());
            }
            "/new" => active = None,
            "/reconcile" => {
                let resumed=sqlx::query("UPDATE tasks SET state='queued',revision=revision+1,updated=?
                    WHERE id=? AND user=? AND state='uncertain'").bind(now()).bind(&active).bind(user)
                    .execute(&mut *tx).await?.rows_affected()==1;
                if resumed {
                    sqlx::query("INSERT INTO events(task,role,body) VALUES(?,'user','I checked the external destination. Continue from here and do not automatically retry the earlier action.')")
                        .bind(&active).execute(&mut *tx).await?;
                }
                response=Some(if resumed {"Task resumed after reconciliation."} else {"The active task does not need reconciliation."}.into());
            }
            "/use" => {
                let agent: Option<String> = sqlx::query_scalar("SELECT name FROM agents a WHERE a.id=? AND
                    (a.owner=? OR EXISTS(SELECT 1 FROM grants g WHERE g.agent=a.id AND g.member=?))")
                    .bind(argument).bind(user).bind(user).fetch_optional(&mut *tx).await?;
                response = Some(if let Some(name) = agent {
                    let task = id();
                    sqlx::query("INSERT INTO tasks(id,user,agent,title,state,updated) VALUES(?,?,?,?,'idle',?)")
                        .bind(&task).bind(user).bind(argument).bind(format!("Ask {name}")).bind(now()).execute(&mut *tx).await?;
                    sqlx::query("UPDATE users SET active_task=? WHERE id=?").bind(task).bind(user).execute(&mut *tx).await?;
                    format!("Send your question for {name}. Use /new to return to your Chief.")
                } else { "Agent unavailable.".into() });
            }
            _ if text.is_empty() => response = Some("Send a text message to give TinyBot a task.".into()),
            _ if command.starts_with('/') => response = Some("Use /help to see available commands.".into()),
            _ => {}
        }
    }
    if let Some(response) = response {
        sqlx::query("INSERT INTO outbox(user,body,button) VALUES(?,?,'mini')")
            .bind(user)
            .bind(response)
            .execute(&mut *tx)
            .await?;
    } else {
        let input = if command == "/new" { argument } else { text };
        let task = if let Some(active) = active {
            active
        } else {
            let task = id();
            sqlx::query("INSERT INTO tasks(id,user,title,goal,updated) VALUES(?,?,?,?,?)")
                .bind(&task)
                .bind(user)
                .bind(clip(input, 100))
                .bind(clip(input, 1500))
                .bind(now())
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE users SET active_task=? WHERE id=?")
                .bind(&task)
                .bind(user)
                .execute(&mut *tx)
                .await?;
            task
        };
        let updated:Option<String> = sqlx::query_scalar("UPDATE tasks SET state=CASE WHEN state IN ('running','uncertain') THEN state ELSE ? END,
            revision=revision+1,updated=? WHERE id=? AND user=? AND state NOT IN ('cancelled','revoked') AND
            (agent IS NULL OR EXISTS(SELECT 1 FROM agents a WHERE a.id=tasks.agent AND
            (a.owner=? OR EXISTS(SELECT 1 FROM grants g WHERE g.agent=a.id AND g.member=?)))) RETURNING state")
            .bind(if provider.is_some() {"queued"} else {"setup"}).bind(now()).bind(&task).bind(user)
            .bind(user).bind(user).fetch_optional(&mut *tx).await?;
        if let Some(state) = updated {
            sqlx::query("UPDATE pending SET state='superseded' WHERE task=? AND state IN ('waiting','approved')")
                .bind(&task).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO events(task,role,body) VALUES (?,'user',?)")
                .bind(&task)
                .bind(input)
                .execute(&mut *tx)
                .await?;
            if state == "uncertain" {
                sqlx::query("INSERT INTO outbox(user,body) VALUES(?,?)").bind(user)
                    .bind("I saved your message. Check the last action at its destination, then send /reconcile to continue.")
                    .execute(&mut *tx).await?;
            } else if provider.is_none() {
                sqlx::query("INSERT INTO outbox(user,body,button) VALUES(?,?,'mini')")
                    .bind(user)
                    .bind("I saved your task. Choose a model under Settings & forms and I’ll pick it up.")
                    .execute(&mut *tx)
                    .await?;
            }
        } else {
            sqlx::query("INSERT INTO outbox(user,body) VALUES(?,?)")
                .bind(user)
                .bind("This task is unavailable. Use /new to start another task.")
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}
