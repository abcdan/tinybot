mod schema;

use crate::{
    security::{id, now},
    store::{self, Pending, Task},
};
use anyhow::{Context, Result, ensure};
#[cfg(test)]
pub use schema::Field;
use schema::validate_values;
pub use schema::{FormSpec, WebsiteForm};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use std::collections::{BTreeMap, HashMap};
use tokio::sync::Mutex;

struct PrivateRequest {
    task: Task,
    form: WebsiteForm,
    expires: i64,
}

struct SealedSubmission {
    values: BTreeMap<String, String>,
    form: WebsiteForm,
    expires: i64,
}

#[derive(Default)]
pub struct Forms {
    requests: Mutex<HashMap<String, PrivateRequest>>,
    submissions: Mutex<HashMap<String, SealedSubmission>>,
}

impl Forms {
    pub async fn request(&self, db: &SqlitePool, task: &Task, form: WebsiteForm) -> Result<()> {
        let pending_id = id();
        let expires = now() + 600;
        let target = form.destination.clone();
        let payload = json!({"destination":target,"fields":form.fields});
        self.requests.lock().await.insert(
            pending_id.clone(),
            PrivateRequest {
                task: task.clone(),
                form,
                expires,
            },
        );
        let notification = format!(
            "I prepared a form for {target}. Open TinyBot in Telegram to enter your details. The form expires in 10 minutes."
        );
        if let Err(error) = store::pause(
            db,
            task,
            "form",
            &payload,
            &notification,
            Some("mini"),
            600,
            &pending_id,
        )
        .await
        {
            self.requests.lock().await.remove(&pending_id);
            return Err(error);
        }
        Ok(())
    }

    pub async fn receive(
        &self,
        db: &SqlitePool,
        user: &str,
        pending_id: &str,
        values: BTreeMap<String, String>,
    ) -> Result<()> {
        let mut requests = self.requests.lock().await;
        let request = requests.get(pending_id).context("form expired")?;
        ensure!(
            request.expires >= now()
                && request.task.user == user
                && store::current(db, &request.task).await?,
            "form is unavailable"
        );
        validate_values(&request.form.fields, &values)?;
        let task = &request.task;
        let target = &request.form.destination;
        let preview = request
            .form
            .fields
            .iter()
            .filter_map(|field| {
                let value = values.get(&field.name)?;
                if value.is_empty() {
                    return None;
                }
                Some(format!("{}: ••••", field.label))
            })
            .collect::<Vec<_>>()
            .join("\n");
        let approval = id();
        let mut submissions = self.submissions.lock().await;
        let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
        let changed=sqlx::query("UPDATE pending SET state='done' WHERE id=? AND state='waiting' AND expires>=? AND EXISTS
            (SELECT 1 FROM tasks t WHERE t.id=pending.task AND t.revision=pending.revision AND t.state='waiting')")
            .bind(pending_id).bind(now()).execute(&mut *tx).await?.rows_affected();
        ensure!(changed == 1, "form was replaced or already submitted");
        // Store only redacted preview data in SQLite. Full values stay in the sealed in-memory request.
        sqlx::query("INSERT INTO pending(id,user,task,revision,kind,payload,expires) VALUES(?,?,?,?,'form-submit',?,?)")
            .bind(&approval).bind(&task.user).bind(&task.id).bind(task.revision)
            .bind(json!({"destination":target,"preview":preview}).to_string()).bind(now()+600).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO outbox(user,task,agent,body,button) VALUES(?,?,?,?,'mini')")
            .bind(&task.user).bind(&task.id).bind(&task.agent)
            .bind(format!("The form is ready. Review the destination and masked preview in the Mini App before submitting.\n{target}"))
            .execute(&mut *tx).await?;
        submissions.insert(
            approval.clone(),
            SealedSubmission {
                values,
                form: request.form.clone(),
                expires: now() + 600,
            },
        );
        if let Err(error) = tx.commit().await {
            submissions.remove(&approval);
            return Err(error.into());
        }
        requests.remove(pending_id);
        Ok(())
    }

    pub async fn submission(&self, approval: &str) -> Result<Value> {
        let request = self
            .submissions
            .lock()
            .await
            .remove(approval)
            .context("private form expired")?;
        ensure!(request.expires >= now(), "private form expired");
        Ok(json!({"form":request.form,"values":request.values}))
    }

    pub async fn prune(&self, db: &SqlitePool) -> Result<()> {
        let mut requests = self.requests.lock().await;
        requests.retain(|_, request| request.expires >= now());
        let mut submissions = self.submissions.lock().await;
        submissions.retain(|_, request| request.expires >= now());
        for (values, state) in [
            (requests.keys().cloned().collect::<Vec<_>>(), "waiting"),
            (
                submissions.keys().cloned().collect::<Vec<_>>(),
                "waiting|approved",
            ),
        ] {
            for id in values {
                let live:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pending WHERE id=? AND
                    (state='waiting' OR (?='waiting|approved' AND state IN ('approved','executing'))))")
                    .bind(&id).bind(state).fetch_one(db).await?;
                if !live {
                    requests.remove(&id);
                    submissions.remove(&id);
                }
            }
        }
        Ok(())
    }
}

pub async fn answer(
    db: &SqlitePool,
    user: &str,
    request: &str,
    values: BTreeMap<String, String>,
) -> Result<()> {
    submit_answer(db, user, request, |_| Ok(values)).await
}

pub async fn widget_choice(db: &SqlitePool, user: &str, request: &str) -> Result<Value> {
    let pending = waiting_input(db, user, request).await?;
    let spec: FormSpec = serde_json::from_str(&pending.payload)?;
    spec.validate()?;
    let field = spec
        .widget_choice()
        .context("answer this form in Settings & forms")?;
    Ok(json!({"id":pending.id,"label":field.label,"options":field.options}))
}

pub async fn answer_choice(
    db: &SqlitePool,
    user: &str,
    request: &str,
    option: usize,
) -> Result<()> {
    submit_answer(db, user, request, |spec| {
        let field = spec
            .widget_choice()
            .context("answer this form in Settings & forms")?;
        let value = field.options.get(option).context("invalid choice")?;
        Ok(BTreeMap::from([(field.name.clone(), value.clone())]))
    })
    .await
}

async fn waiting_input<'a>(
    db: impl sqlx::Executor<'a, Database = sqlx::Sqlite>,
    user: &str,
    request: &str,
) -> Result<Pending> {
    sqlx::query_as(
        "SELECT p.* FROM pending p JOIN tasks t ON t.id=p.task AND t.user=p.user
        WHERE p.id=? AND p.user=? AND p.kind='elicitation' AND p.state='waiting' AND p.expires>=?
        AND t.revision=p.revision AND t.state='waiting'
        AND (t.agent IS NULL OR EXISTS(SELECT 1 FROM agents a WHERE a.id=t.agent AND a.owner=p.user))",
    )
    .bind(request)
    .bind(user)
    .bind(now())
    .fetch_optional(db)
    .await?
    .context("form expired or unavailable")
}

async fn submit_answer(
    db: &SqlitePool,
    user: &str,
    request: &str,
    values: impl FnOnce(&FormSpec) -> Result<BTreeMap<String, String>>,
) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let pending = waiting_input(&mut *tx, user, request).await?;
    let spec: FormSpec = serde_json::from_str(&pending.payload)?;
    spec.validate()?;
    let values = values(&spec)?;
    validate_values(&spec.fields, &values)?;
    sqlx::query("UPDATE pending SET state='done' WHERE id=?")
        .bind(request)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO events(task,role,body) VALUES(?,'user',?)")
        .bind(&pending.task)
        .bind(format!(
            "My answers to your form: {}",
            serde_json::to_string(&values)?
        ))
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE tasks SET state='queued',revision=revision+1,updated=? WHERE id=?")
        .bind(now())
        .bind(&pending.task)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
