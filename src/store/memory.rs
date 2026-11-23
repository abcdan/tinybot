use super::Task;
use crate::security::{id, now};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{FromRow, SqliteConnection, SqlitePool};

#[cfg(test)]
mod tests;

const PAGE_SIZE: i64 = 30;
const CONTEXT_LIMIT: i64 = 16;
const RECALL_LIMIT: i64 = 8;
const CANDIDATE_LIMIT: i64 = 24;
const ARCHIVE_LIMIT: usize = 8;
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

#[derive(Debug, FromRow, Serialize)]
pub(crate) struct MemoryCandidate {
    pub id: String,
    pub body: String,
    pub scope: Option<MemoryScope>,
    pub automatic: bool,
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
         WHERE owner=? AND scope=? AND agent IS ? AND archived_at IS NULL
         AND (? IS NULL OR sequence < ?)
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

pub(crate) async fn candidates(
    db: &SqlitePool,
    task: &Task,
    helper: Option<&str>,
    request: &str,
) -> Result<Vec<MemoryCandidate>> {
    let mut keywords = Vec::new();
    for word in request.split(|character: char| !character.is_alphanumeric()) {
        if (3..=64).contains(&word.len()) {
            let word = word.to_lowercase();
            if !keywords.contains(&word) {
                keywords.push(word);
                if keywords.len() == 12 {
                    break;
                }
            }
        }
    }
    read_notes(db, task, helper, "", &keywords, CANDIDATE_LIMIT).await
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

pub(crate) async fn apply_automatic(
    db: &SqlitePool,
    task: &Task,
    helper: Option<&str>,
    group: bool,
    body: Option<&str>,
    archive_ids: &[String],
) -> Result<bool> {
    ensure!(
        archive_ids.len() <= ARCHIVE_LIMIT,
        "too many memory updates"
    );
    ensure!(
        !group || helper.is_none(),
        "group memory requires the Chief"
    );
    let body = body.map(validated_body).transpose()?;
    if body.is_none() && archive_ids.is_empty() {
        return Ok(false);
    }
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let (scope, agent) = match audience(&mut tx, task, helper).await? {
        Some(Audience::Chief) if group => (MemoryScope::Group, None),
        Some(Audience::Chief) => (MemoryScope::Chief, None),
        Some(Audience::OwnedHelper(agent)) => (MemoryScope::Helper, Some(agent)),
        _ => return Ok(false),
    };
    let ids = json!(archive_ids).to_string();
    let time = now();
    if !archive_ids.is_empty() {
        let candidates: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_memory
             WHERE owner=? AND scope=? AND agent IS ? AND automatic=1 AND archived_at IS NULL
               AND id IN (SELECT value FROM json_each(?))",
        )
        .bind(&task.user)
        .bind(scope)
        .bind(agent)
        .bind(&ids)
        .fetch_one(&mut *tx)
        .await?;
        if candidates != archive_ids.len() as i64 {
            return Ok(false);
        }
    }
    let mut changed = false;
    if let Some(body) = body {
        changed = sqlx::query(
            "INSERT INTO agent_memory(id,owner,scope,agent,body,created_at,automatic)
             VALUES (?,?,?,?,?,?,1) ON CONFLICT DO NOTHING",
        )
        .bind(id())
        .bind(&task.user)
        .bind(scope)
        .bind(agent)
        .bind(body)
        .bind(time)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
    }
    if !archive_ids.is_empty() {
        changed |= sqlx::query(
            "UPDATE agent_memory SET archived_at=?
             WHERE id IN (SELECT value FROM json_each(?)) AND body IS NOT ?",
        )
        .bind(time)
        .bind(ids)
        .bind(body)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
    }
    tx.commit().await?;
    Ok(changed)
}

async fn save_entry(
    connection: &mut SqliteConnection,
    owner: &str,
    scope: MemoryScope,
    agent: Option<&str>,
    body: &str,
) -> Result<MemoryEntry> {
    let body = validated_body(body)?;
    Ok(sqlx::query_as(
        "INSERT INTO agent_memory(id,owner,scope,agent,body,created_at) VALUES (?,?,?,?,?,?)
         ON CONFLICT(owner,scope,coalesce(agent,''),body) WHERE archived_at IS NULL
         DO UPDATE SET automatic=0
         RETURNING sequence,id,scope,agent,body,created_at",
    )
    .bind(id())
    .bind(owner)
    .bind(scope)
    .bind(agent)
    .bind(body)
    .bind(now())
    .fetch_one(connection)
    .await?)
}

fn validated_body(body: &str) -> Result<&str> {
    let body = body.trim();
    ensure!(
        !body.is_empty() && body.len() <= MAX_NOTE_BYTES && !body.contains('\0'),
        "memory needs text of up to {MAX_NOTE_BYTES} bytes"
    );
    Ok(body)
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
    Ok(read_notes(db, task, helper, query, &[], limit)
        .await?
        .into_iter()
        .map(|candidate| candidate.body)
        .collect())
}

async fn read_notes(
    db: &SqlitePool,
    task: &Task,
    helper: Option<&str>,
    query: &str,
    keywords: &[String],
    limit: i64,
) -> Result<Vec<MemoryCandidate>> {
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
    let notes = sqlx::query_as(
        "WITH notes AS (
             SELECT id,body,scope,automatic,sequence,0 AS published FROM agent_memory
             WHERE owner=? AND archived_at IS NULL AND ((scope=? AND agent IS ?) OR (? AND scope='group'))
             UNION ALL
             SELECT id,body,NULL,0,rowid,1 AS published FROM memory WHERE owner=? AND agent=?
         )
         SELECT id,body,scope,automatic FROM notes WHERE instr(lower(body), lower(?))>0
         ORDER BY (SELECT COUNT(*) FROM json_each(?) WHERE instr(lower(notes.body), value)>0) DESC,
                  published, sequence DESC LIMIT ?",
    )
    .bind(&task.user)
    .bind(scope)
    .bind(agent)
    .bind(scope.is_some())
    .bind(published_owner)
    .bind(agent)
    .bind(query)
    .bind(json!(keywords).to_string())
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(notes)
}
