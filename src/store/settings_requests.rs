use super::{Outbound, Pending, Task, requests::pause_in, settings_history as history};
use crate::{
    App, audit,
    security::{id, now},
    settings::{self, Change, Key, Snapshot},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Executor, Sqlite, SqlitePool};
use std::collections::{BTreeMap, BTreeSet};

const FORM: &str = "settings-form";
const APPROVAL: &str = "settings-approve";
const LIFETIME: i64 = 600;
pub(crate) const BUTTON_PREFIX: &str = "settings-form/";

pub(super) const TASK_SCOPE: &str = "t.chief_chat=1 AND t.agent IS NULL
    AND t.shared_memory_pair IS NULL AND t.schedule IS NULL
    AND EXISTS(SELECT 1 FROM users u WHERE u.id=t.user AND u.suspended_at IS NULL)
    AND NOT EXISTS(SELECT 1 FROM mail_messages m WHERE m.task=t.id)
    AND NOT EXISTS(SELECT 1 FROM telegram_group_tasks g WHERE g.task=t.id)
    AND ((SELECT e.telegram_message_id FROM events e
          WHERE e.task=t.id AND e.role='user' ORDER BY e.id DESC LIMIT 1)>0
         OR EXISTS(SELECT 1 FROM settings_rollbacks r WHERE r.task=t.id))";

#[derive(Debug)]
pub(crate) struct NoChanges;

impl NoChanges {
    pub(crate) const MESSAGE: &'static str =
        "These settings already have those values. Change a value to continue.";
}

impl std::fmt::Display for NoChanges {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str(Self::MESSAGE)
    }
}

impl std::error::Error for NoChanges {}

#[derive(Serialize, Deserialize)]
struct Draft {
    keys: Vec<Key>,
    before: Snapshot,
    prefill: BTreeMap<Key, String>,
}

pub(crate) fn is_settings_kind(kind: &str) -> bool {
    matches!(kind, FORM | APPROVAL)
}

pub(crate) async fn permitted(db: &SqlitePool, task: &Task) -> Result<bool> {
    permitted_on(db, task).await
}

async fn permitted_on<'a>(db: impl Executor<'a, Database = Sqlite>, task: &Task) -> Result<bool> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT EXISTS(SELECT 1 FROM tasks t WHERE t.id=? AND t.user=?
         AND t.revision=? AND t.state='running' AND {TASK_SCOPE}
         AND NOT EXISTS(SELECT 1 FROM settings_rollbacks WHERE task=t.id))"
    ))
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .fetch_one(db)
    .await?)
}

pub(crate) async fn propose(
    app: &App,
    task: &Task,
    fields: Vec<Key>,
    values: BTreeMap<Key, String>,
) -> Result<()> {
    let form = settings::form(&fields)?;
    ensure!(
        values.iter().all(|(key, value)| {
            form.fields.iter().any(|field| {
                field.name == key.as_str()
                    && value.len() <= 256
                    && !value.chars().any(char::is_control)
                    && (field.options.is_empty() || field.options.contains(value))
            })
        }),
        "invalid settings form defaults"
    );
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await?;
    ensure!(
        permitted_on(&mut *tx, task).await?,
        "settings are available in the personal Chief chat"
    );
    let declined: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pending WHERE task=? AND revision=? AND state='denied'
         AND kind IN ('settings-form','settings-approve'))",
    )
    .bind(&task.id)
    .bind(task.revision)
    .fetch_one(&mut *tx)
    .await?;
    ensure!(
        !declined,
        "The user declined these settings changes. Wait for a new request."
    );
    let draft = Draft {
        keys: fields,
        before: settings::snapshot(&mut tx, &task.user).await?,
        prefill: values,
    };
    let request = id();
    pause_in(
        &mut tx,
        task,
        FORM,
        &serde_json::to_value(draft)?,
        "Open the form to review the proposed TinyBot settings. You can adjust the values before approving.",
        Some(&format!("{BUTTON_PREFIX}{request}")),
        LIFETIME,
        &request,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn waiting<'a>(
    db: impl Executor<'a, Database = Sqlite>,
    user: &str,
    request: &str,
) -> Result<Option<Pending>> {
    Ok(sqlx::query_as(&format!(
        "SELECT p.* FROM pending p JOIN tasks t ON t.id=p.task AND t.user=p.user
         WHERE p.id=? AND p.user=? AND p.state='waiting' AND p.expires>=?
           AND p.kind IN ('settings-form','settings-approve')
           AND t.state='waiting' AND t.revision=p.revision AND {TASK_SCOPE}"
    ))
    .bind(request)
    .bind(user)
    .bind(now())
    .fetch_optional(db)
    .await?)
}

pub(super) async fn waiting_delivery(
    db: &SqlitePool,
    item: &Outbound,
    request: &str,
) -> Result<bool> {
    Ok(waiting(db, &item.user, request)
        .await?
        .is_some_and(|pending| pending.task == item.task))
}

pub(crate) async fn get(db: &SqlitePool, user: &str, request: &str) -> Result<Option<Value>> {
    waiting(db, user, request)
        .await?
        .as_ref()
        .map(view)
        .transpose()
}

pub(crate) fn view(pending: &Pending) -> Result<Value> {
    let content = match pending.kind.as_str() {
        FORM => {
            let draft: Draft = serde_json::from_str(&pending.payload)?;
            let form = settings::form(&draft.keys)?;
            let fields = form
                .fields
                .into_iter()
                .map(|field| {
                    let key = draft
                        .keys
                        .iter()
                        .copied()
                        .find(|key| key.as_str() == field.name)
                        .context("settings field unavailable")?;
                    let mut data = serde_json::to_value(field)?;
                    data["value"] = json!(
                        draft
                            .prefill
                            .get(&key)
                            .map_or_else(|| draft.before.value(key), String::as_str)
                    );
                    Ok(data)
                })
                .collect::<Result<Vec<Value>>>()?;
            json!({"operation":"Configure TinyBot","fields":fields})
        }
        APPROVAL => {
            let change: Change = serde_json::from_str(&pending.payload)?;
            json!({"operation":"Review TinyBot settings","changes":change.preview()})
        }
        _ => anyhow::bail!("settings form unavailable"),
    };
    Ok(json!({"id":pending.id,"kind":pending.kind,"content":content,"expires":pending.expires}))
}

pub(crate) async fn answer(
    app: &App,
    user: &str,
    request: &str,
    values: BTreeMap<Key, String>,
) -> Result<Value> {
    let pending = waiting(&app.db, user, request)
        .await?
        .filter(|pending| pending.kind == FORM)
        .context("settings form expired or unavailable")?;
    let draft: Draft = serde_json::from_str(&pending.payload)?;
    ensure!(
        values.keys().copied().collect::<BTreeSet<_>>() == draft.keys.into_iter().collect(),
        "answer the requested settings fields"
    );
    let change = settings::prepare(app, user, &values, &draft.before).await?;
    ensure!(!change.preview().is_empty(), NoChanges);
    let task = pending
        .task
        .as_deref()
        .context("settings form unavailable")?;
    let revision = pending.revision.context("settings form unavailable")?;
    let approval = Pending {
        id: id(),
        task: pending.task.clone(),
        revision: pending.revision,
        kind: APPROVAL.to_owned(),
        payload: serde_json::to_string(&change)?,
        expires: now() + LIFETIME,
    };

    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await?;
    ensure!(
        waiting(&mut *tx, user, request)
            .await?
            .is_some_and(|fresh| fresh.kind == FORM),
        "settings form expired or unavailable"
    );
    sqlx::query("UPDATE pending SET state='done',payload='{}' WHERE id=?")
        .bind(request)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO pending(id,user,task,revision,kind,payload,expires) VALUES(?,?,?,?,?,?,?)",
    )
    .bind(&approval.id)
    .bind(user)
    .bind(task)
    .bind(revision)
    .bind(APPROVAL)
    .bind(&approval.payload)
    .bind(approval.expires)
    .execute(&mut *tx)
    .await?;
    audit::request_changed(&mut tx, task, revision, FORM, "answered").await?;
    sqlx::query("INSERT INTO outbox(user,task,body,button) VALUES(?,?,?,?)")
        .bind(user)
        .bind(task)
        .bind("Review the exact TinyBot settings changes before saving them.")
        .bind(format!("{BUTTON_PREFIX}{}", approval.id))
        .execute(&mut *tx)
        .await?;
    let review = view(&approval)?;
    tx.commit().await?;
    Ok(review)
}

pub(crate) async fn approve(app: &App, user: &str, request: &str) -> Result<()> {
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await?;
    let pending = waiting(&mut *tx, user, request)
        .await?
        .filter(|pending| pending.kind == APPROVAL)
        .context("settings approval expired or unavailable")?;
    let task = pending
        .task
        .as_deref()
        .context("settings approval unavailable")?;
    let revision = pending.revision.context("settings approval unavailable")?;
    let change: Change = serde_json::from_str(&pending.payload)?;
    let restored_from = history::origin(&mut tx, task).await?;
    if let Some(original) = restored_from {
        let current = settings::snapshot(&mut tx, user).await?;
        let values = history::restore_target(&mut tx, user, original, &current).await?;
        ensure!(
            change.preview().len() == values.len()
                && change
                    .personal_values()
                    .map(|(key, _, after)| (key, after))
                    .eq(values.iter().map(|(&key, value)| (key, value.as_str()))),
            settings::Stale
        );
    }
    settings::apply(&mut tx, user, &change).await?;
    let source = if restored_from.is_some() {
        "restore"
    } else {
        "approval"
    };
    history::record(
        &mut tx,
        user,
        restored_from,
        source,
        change.personal_values(),
    )
    .await?;
    sqlx::query("UPDATE pending SET state='done',payload='{}' WHERE id=?")
        .bind(request)
        .execute(&mut *tx)
        .await?;
    audit::request_changed(&mut tx, task, revision, APPROVAL, "approved").await?;
    let message = "I saved the approved TinyBot settings.";
    sqlx::query("INSERT INTO events(task,role,body) VALUES(?,'tool',?)")
        .bind(task)
        .bind("The user approved and TinyBot saved the exact settings changes. No further action is required for this request.")
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO outbox(user,task,body) VALUES(?,?,?)")
        .bind(user)
        .bind(task)
        .bind(message)
        .execute(&mut *tx)
        .await?;
    if restored_from.is_some() {
        history::close_review(&mut tx, task, revision).await?;
    } else {
        sqlx::query("UPDATE tasks SET state='queued',updated=? WHERE id=?")
            .bind(now())
            .bind(task)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn propose_restore(app: &App, user: &str, original: i64) -> Result<Value> {
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await?;
    let current = settings::snapshot(&mut tx, user).await?;
    let values = history::restore_target(&mut tx, user, original, &current).await?;
    let existing: Option<Pending> = sqlx::query_as(
        "SELECT p.* FROM settings_rollbacks r JOIN pending p ON p.task=r.task
         JOIN tasks t ON t.id=p.task AND t.revision=p.revision AND t.user=p.user
         WHERE r.change_id=? AND p.user=? AND p.state='waiting' AND p.kind='settings-approve'
           AND p.expires>=? AND t.state='waiting' ORDER BY p.expires DESC LIMIT 1",
    )
    .bind(original)
    .bind(user)
    .bind(now())
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(existing) = existing {
        return view(&existing);
    }
    let change = settings::prepare_values(&values, &current)?;
    ensure!(!change.preview().is_empty(), NoChanges);
    let task: Task = sqlx::query_as(
        "INSERT INTO tasks(id,user,title,state,chief_chat,updated)
         VALUES(?,?,'Restore personal settings','running',1,?) RETURNING *",
    )
    .bind(id())
    .bind(user)
    .bind(now())
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO settings_rollbacks(task,change_id) VALUES(?,?)")
        .bind(&task.id)
        .bind(original)
        .execute(&mut *tx)
        .await?;
    let request = id();
    audit::open(&mut tx, &task.id, "Restore personal settings").await?;
    pause_in(
        &mut tx,
        &task,
        APPROVAL,
        &serde_json::to_value(&change)?,
        "Review the exact changes before restoring your TinyBot settings.",
        Some(&format!("{BUTTON_PREFIX}{request}")),
        LIFETIME,
        &request,
    )
    .await?;
    let review = waiting(&mut *tx, user, &request)
        .await?
        .context("settings review unavailable")?;
    let review = view(&review)?;
    tx.commit().await?;
    Ok(review)
}
