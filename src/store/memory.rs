use super::Task;
use crate::security::{id, now};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqliteConnection, SqlitePool};

#[cfg(test)]
mod tests;

const PAGE_SIZE: i64 = 30;
const CONTEXT_LIMIT: i64 = 16;
const RECALL_LIMIT: i64 = 8;
const MAX_NOTE_BYTES: usize = 1200;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum MemoryScope {
    Chief,
    Helper,
    Group,
}

#[derive(Debug, FromRow, Serialize)]
pub struct MemoryEntry {
    #[serde(skip)]
    sequence: i64,
    pub id: String,
    pub scope: MemoryScope,
    pub agent: Option<String>,
    pub body: String,
    pub created_at: i64,
}

pub async fn entries(
    db: &SqlitePool,
    owner: &str,
    scope: MemoryScope,
    agent: Option<&str>,
    before: Option<i64>,
) -> Result<(Vec<MemoryEntry>, Option<i64>)> {
    validate_scope(scope, agent)?;
    let mut notes: Vec<MemoryEntry> = sqlx::query_as(
        "SELECT sequence,id,scope,agent,body,created_at FROM agent_memory
         WHERE owner=? AND scope=? AND agent IS ? AND (? IS NULL OR sequence < ?)
         ORDER BY sequence DESC LIMIT ?",
    )
    .bind(owner)
    .bind(scope)
    .bind(agent)
    .bind(before)
    .bind(before)
    .bind(PAGE_SIZE + 1)
    .fetch_all(db)
    .await?;
    let next = (notes.len() > PAGE_SIZE as usize).then(|| notes[PAGE_SIZE as usize - 1].sequence);
    notes.truncate(PAGE_SIZE as usize);
    Ok((notes, next))
}

pub async fn save(
    db: &SqlitePool,
    owner: &str,
    scope: MemoryScope,
    agent: Option<&str>,
    body: &str,
) -> Result<MemoryEntry> {
    validate_scope(scope, agent)?;
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    if let Some(agent) = agent {
        ensure!(
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM agents WHERE id=? AND owner=?)"
            )
            .bind(agent)
            .bind(owner)
            .fetch_one(&mut *tx)
            .await?,
            "memory helper unavailable"
        );
    }
    let entry = save_entry(&mut tx, owner, scope, agent, body).await?;
    tx.commit().await?;
    Ok(entry)
}

pub async fn remove(db: &SqlitePool, owner: &str, id: &str) -> Result<bool> {
    Ok(
        sqlx::query("DELETE FROM agent_memory WHERE id=? AND owner=?")
            .bind(id)
            .bind(owner)
            .execute(db)
            .await?
            .rows_affected()
            == 1,
    )
}

pub async fn chief_context(db: &SqlitePool, task: &Task) -> Result<Vec<String>> {
    read(db, task, None, "", CONTEXT_LIMIT).await
}

pub async fn helper_context(
    db: &SqlitePool,
    parent: &Task,
    helper: Option<&str>,
) -> Result<Vec<String>> {
    let Some(helper) = helper else {
        return Ok(Vec::new());
    };
    read(db, parent, Some(helper), "", CONTEXT_LIMIT).await
}

pub async fn recall(
    db: &SqlitePool,
    task: &Task,
    helper: Option<&str>,
    query: &str,
) -> Result<Vec<String>> {
    let query = query.trim();
    ensure!(
        !query.is_empty() && query.len() <= 160,
        "memory search needs a query of up to 160 bytes"
    );
    read(db, task, helper, query, RECALL_LIMIT).await
}

pub async fn remember(
    db: &SqlitePool,
    task: &Task,
    helper: Option<&str>,
    group: bool,
    body: &str,
) -> Result<MemoryEntry> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let audience = audience(&mut tx, task, helper)
        .await?
        .context("task or agent changed before saving memory")?;
    let (scope, agent) = match (audience, group) {
        (Audience::Chief, false) => (MemoryScope::Chief, None),
        (Audience::OwnedHelper(agent), false) => (MemoryScope::Helper, Some(agent)),
        (Audience::Chief | Audience::OwnedHelper(_), true) => (MemoryScope::Group, None),
        (Audience::Guest { .. }, _) => anyhow::bail!("memory is read-only in a shared helper chat"),
    };
    let entry = save_entry(&mut tx, &task.user, scope, agent, body).await?;
    tx.commit().await?;
    Ok(entry)
}

async fn save_entry(
    connection: &mut SqliteConnection,
    owner: &str,
    scope: MemoryScope,
    agent: Option<&str>,
    body: &str,
) -> Result<MemoryEntry> {
    let body = body.trim();
    ensure!(
        !body.is_empty() && body.len() <= MAX_NOTE_BYTES && !body.contains('\0'),
        "memory needs text of up to {MAX_NOTE_BYTES} bytes"
    );
    sqlx::query(
        "INSERT INTO agent_memory(id,owner,scope,agent,body,created_at)
         VALUES (?,?,?,?,?,?) ON CONFLICT DO NOTHING",
    )
    .bind(id())
    .bind(owner)
    .bind(scope)
    .bind(agent)
    .bind(body)
    .bind(now())
    .execute(&mut *connection)
    .await?;
    Ok(sqlx::query_as(
        "SELECT sequence,id,scope,agent,body,created_at FROM agent_memory
         WHERE owner=? AND scope=? AND agent IS ? AND body=?",
    )
    .bind(owner)
    .bind(scope)
    .bind(agent)
    .bind(body)
    .fetch_one(connection)
    .await?)
}

fn validate_scope(scope: MemoryScope, agent: Option<&str>) -> Result<()> {
    ensure!(
        (scope == MemoryScope::Helper) == agent.is_some(),
        "select a helper only for helper memory"
    );
    Ok(())
}

enum Audience<'a> {
    Chief,
    OwnedHelper(&'a str),
    Guest { agent: &'a str, owner: String },
}

async fn audience<'a>(
    connection: &mut SqliteConnection,
    task: &'a Task,
    helper: Option<&'a str>,
) -> Result<Option<Audience<'a>>> {
    let scope: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT a.owner FROM tasks t LEFT JOIN agents a ON t.agent=a.id
         WHERE t.id=? AND t.user=? AND t.revision=? AND t.agent IS ?
         AND t.state IN ('running','waiting')
         AND (t.agent IS NULL OR a.owner=t.user OR EXISTS(
              SELECT 1 FROM grants g WHERE g.agent=t.agent AND g.member=t.user))",
    )
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .bind(&task.agent)
    .fetch_optional(&mut *connection)
    .await?;
    let Some((owner,)) = scope else {
        return Ok(None);
    };

    if let Some(agent) = task.agent.as_deref() {
        if helper.is_some_and(|helper| helper != agent) {
            return Ok(None);
        }
        if let Some(owner) = owner.filter(|owner| owner != &task.user) {
            return Ok(Some(Audience::Guest { agent, owner }));
        }
    }
    let Some(helper) = helper else {
        return Ok(Some(Audience::Chief));
    };
    let owned =
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM agents WHERE id=? AND owner=?)")
            .bind(helper)
            .bind(&task.user)
            .fetch_one(connection)
            .await?;
    Ok(owned.then_some(Audience::OwnedHelper(helper)))
}

async fn read(
    db: &SqlitePool,
    task: &Task,
    helper: Option<&str>,
    query: &str,
    limit: i64,
) -> Result<Vec<String>> {
    let mut tx = db.begin().await?;
    let Some(audience) = audience(&mut tx, task, helper).await? else {
        return Ok(Vec::new());
    };
    let (scope, agent, published_owner) = match &audience {
        Audience::Chief => (Some(MemoryScope::Chief), None, None),
        Audience::OwnedHelper(agent) => (
            Some(MemoryScope::Helper),
            Some(*agent),
            Some(task.user.as_str()),
        ),
        Audience::Guest { agent, owner } => (None, Some(*agent), Some(owner.as_str())),
    };
    let notes = sqlx::query_scalar(
        "WITH notes AS (
             SELECT body,sequence,0 AS published FROM agent_memory
             WHERE owner=? AND ((scope=? AND agent IS ?) OR (? AND scope='group'))
             UNION ALL
             SELECT body,rowid,1 AS published FROM memory WHERE owner=? AND agent=?
         )
         SELECT body FROM notes WHERE instr(lower(body), lower(?))>0
         ORDER BY published, sequence DESC LIMIT ?",
    )
    .bind(&task.user)
    .bind(scope)
    .bind(agent)
    .bind(scope.is_some())
    .bind(published_owner)
    .bind(agent)
    .bind(query)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(notes)
}
