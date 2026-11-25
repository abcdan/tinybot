use crate::security::{clip, id, now};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};

const INVITATION: &str = "project-invitation/";
const ASSIGNMENT: &str = "project-assignment/";

#[derive(Debug)]
pub(crate) enum ProjectError {
    Unavailable,
    Forbidden,
    Changed,
    Duplicate,
}

impl ProjectError {
    pub(crate) fn message(&self) -> &'static str {
        match self {
            Self::Unavailable => "This project or item is no longer available to your account.",
            Self::Forbidden => "Your account does not have permission for this project action.",
            Self::Changed => {
                "The project or item changed. Refresh it and review the current version."
            }
            Self::Duplicate => "This account already has an invitation or a role in this project.",
        }
    }
}

impl std::fmt::Display for ProjectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for ProjectError {}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(rename_all = "lowercase")]
pub(crate) enum Role {
    Owner,
    Editor,
    Viewer,
}

impl Role {
    fn can_publish(self) -> bool {
        matches!(self, Self::Owner | Self::Editor)
    }
}

#[derive(FromRow, Serialize)]
pub(crate) struct Space {
    id: String,
    title: String,
    role: Role,
    revision: i64,
    joined: bool,
    membership: String,
}

#[derive(FromRow, Serialize)]
pub(crate) struct Member {
    id: String,
    name: String,
    role: Role,
    joined: bool,
    active: bool,
}

#[derive(FromRow, Serialize)]
pub(crate) struct Item {
    id: String,
    title: String,
    body: String,
    kind: String,
    source_revision: Option<i64>,
    created_at: i64,
    assignee: Option<String>,
    assignee_name: Option<String>,
    done: bool,
    revision: i64,
    can_manage: bool,
    can_remove: bool,
    can_complete: bool,
    #[sqlx(skip)]
    eligible: Vec<Member>,
}

#[derive(Serialize)]
pub(crate) struct Detail {
    space: Space,
    members: Vec<Member>,
    items: Vec<Item>,
    next_offset: Option<i64>,
}

#[derive(Deserialize)]
pub(crate) struct Review {
    revision: i64,
    audience: Vec<String>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(crate) enum Source {
    Note {
        title: String,
        body: String,
    },
    Task {
        task: String,
        revision: i64,
        event: i64,
    },
}

#[derive(FromRow, Serialize)]
pub(crate) struct TaskResult {
    task: String,
    title: String,
    revision: i64,
    event: i64,
    body: String,
    truncated: bool,
}

const SPACES: &str = "SELECT p.id,p.title,m.role,p.revision,m.joined_at IS NOT NULL AS joined,
    m.id AS membership FROM project_spaces p JOIN project_members m ON m.project=p.id
    JOIN users owner ON owner.id=p.owner AND owner.suspended_at IS NULL
    JOIN users u ON u.id=m.user AND u.suspended_at IS NULL
    WHERE m.user=?";

pub(crate) async fn list(db: &SqlitePool, user: &str) -> Result<Vec<Space>> {
    Ok(sqlx::query_as(&format!(
        "{SPACES} ORDER BY m.joined_at IS NOT NULL,p.title,p.id"
    ))
    .bind(user)
    .fetch_all(db)
    .await?)
}

pub(crate) async fn create(db: &SqlitePool, user: &str, title: &str) -> Result<String> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let project = id();
    sqlx::query("INSERT INTO project_spaces(id,owner,title,created_at) VALUES(?,?,?,?)")
        .bind(&project)
        .bind(user)
        .bind(title)
        .bind(now())
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO project_members(id,project,user,role,joined_at) VALUES(?,?,?,'owner',?)",
    )
    .bind(id())
    .bind(&project)
    .bind(user)
    .bind(now())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(project)
}

async fn space(tx: &mut Transaction<'_, Sqlite>, user: &str, project: &str) -> Result<Space> {
    Ok(sqlx::query_as(&format!("{SPACES} AND p.id=?"))
        .bind(user)
        .bind(project)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(ProjectError::Unavailable)?)
}

async fn joined(tx: &mut Transaction<'_, Sqlite>, user: &str, project: &str) -> Result<Space> {
    let membership = space(tx, user, project).await?;
    ensure!(membership.joined, ProjectError::Unavailable);
    Ok(membership)
}

async fn members(tx: &mut Transaction<'_, Sqlite>, space: &Space) -> Result<Vec<Member>> {
    Ok(sqlx::query_as(
        "SELECT m.id,COALESCE(u.display_name,u.id) AS name,m.role,m.joined_at IS NOT NULL AS joined,
                a.id IS NOT NULL AS active
         FROM project_members m JOIN users u ON u.id=m.user
         LEFT JOIN active_project_members a ON a.id=m.id
         WHERE m.project=? AND (a.id IS NOT NULL OR ?='owner') ORDER BY m.id",
    )
    .bind(&space.id)
    .bind(space.role)
    .fetch_all(&mut **tx)
    .await?)
}

pub(crate) async fn detail(
    db: &SqlitePool,
    user: &str,
    project: &str,
    offset: i64,
) -> Result<Detail> {
    let mut tx = db.begin().await?;
    let space = joined(&mut tx, user, project).await?;
    let members = members(&mut tx, &space).await?;
    let mut items: Vec<Item> = sqlx::query_as(
        "SELECT i.id,i.title,i.body,i.kind,i.source_revision,i.created_at,a.id AS assignee,
                COALESCE(a.display_name,a.user) AS assignee_name,i.done,i.revision,
                (?3='owner' OR (?3='editor' AND i.author=?4)) AS can_manage,
                (?3='owner' OR i.author=?4) AS can_remove,
                (COALESCE(a.id=?2,0) OR ?3='owner' OR (?3='editor' AND i.author=?4)) AS can_complete
         FROM project_items i JOIN project_item_audience allowed ON allowed.item=i.id
         LEFT JOIN active_project_members a ON a.id=i.assignee
         WHERE i.project=?1 AND allowed.member=?2
         ORDER BY i.created_at DESC,i.id LIMIT 26 OFFSET ?5",
    )
    .bind(project)
    .bind(&space.membership)
    .bind(space.role)
    .bind(user)
    .bind(offset)
    .fetch_all(&mut *tx)
    .await?;
    let next_offset = (items.len() > 25).then_some(offset + 25);
    items.truncate(25);
    for item in items.iter_mut().filter(|item| item.can_manage) {
        item.eligible = sqlx::query_as(
            "SELECT a.id,COALESCE(a.display_name,a.user) AS name,a.role,1 AS joined,1 AS active
             FROM project_item_audience allowed JOIN active_project_members a ON a.id=allowed.member
             WHERE allowed.item=? ORDER BY name,a.id",
        )
        .bind(&item.id)
        .fetch_all(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Detail {
        space,
        members,
        items,
        next_offset,
    })
}

async fn refresh_audience(tx: &mut Transaction<'_, Sqlite>, project: &str) -> Result<()> {
    sqlx::query("UPDATE project_spaces SET revision=revision+1 WHERE id=?")
        .bind(project)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum MemberChange {
    Invite { user: String, role: Role },
    Role { member: String, role: Role },
    Revoke { member: String },
    Accept { member: String, role: Role },
    Leave { member: String },
}

pub(crate) async fn update_member(
    db: &SqlitePool,
    user: &str,
    project: &str,
    change: MemberChange,
) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let actor = space(&mut tx, user, project).await?;
    match change {
        MemberChange::Invite { user, role } => {
            ensure!(
                actor.role == Role::Owner && role != Role::Owner,
                ProjectError::Forbidden
            );
            ensure!(
                user.parse::<u64>().is_ok() && user.len() <= 20,
                ProjectError::Unavailable
            );
            let member = id();
            let added = sqlx::query(
                "INSERT INTO project_members(id,project,user,role)
                 SELECT ?,?,u.id,? FROM users u WHERE u.id=? AND u.suspended_at IS NULL
                 AND (SELECT COUNT(*) FROM project_members WHERE project=?)<40
                 ON CONFLICT(project,user) DO NOTHING",
            )
            .bind(&member)
            .bind(project)
            .bind(role)
            .bind(&user)
            .bind(project)
            .execute(&mut *tx)
            .await?;
            if added.rows_affected() == 0 {
                let exists: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM project_members WHERE project=? AND user=?)",
                )
                .bind(project)
                .bind(&user)
                .fetch_one(&mut *tx)
                .await?;
                if exists {
                    return Err(ProjectError::Duplicate.into());
                }
                return Err(ProjectError::Unavailable.into());
            }
            notice(
                &mut tx,
                &user,
                INVITATION,
                &member,
                "You have a project invitation. Review it under Memory → Project spaces.",
            )
            .await?;
        }
        MemberChange::Role { member, role } => {
            ensure!(
                actor.role == Role::Owner && role != Role::Owner,
                ProjectError::Forbidden
            );
            let changed = sqlx::query(
                "UPDATE project_members SET role=? WHERE project=? AND id=? AND role<>'owner'",
            )
            .bind(role)
            .bind(project)
            .bind(member)
            .execute(&mut *tx)
            .await?;
            ensure!(changed.rows_affected() == 1, ProjectError::Unavailable);
        }
        MemberChange::Revoke { member } => {
            ensure!(actor.role == Role::Owner, ProjectError::Forbidden);
            remove_member(&mut tx, project, &member).await?;
        }
        MemberChange::Accept { member, role } => {
            ensure!(
                member == actor.membership && role == actor.role,
                ProjectError::Changed
            );
            if actor.joined {
                return Ok(());
            }
            sqlx::query("UPDATE project_members SET joined_at=? WHERE id=?")
                .bind(now())
                .bind(&actor.membership)
                .execute(&mut *tx)
                .await?;
        }
        MemberChange::Leave { member } => {
            ensure!(member == actor.membership, ProjectError::Changed);
            ensure!(actor.role != Role::Owner, ProjectError::Forbidden);
            remove_member(&mut tx, project, &actor.membership).await?;
        }
    }
    refresh_audience(&mut tx, project).await?;
    tx.commit().await?;
    Ok(())
}

async fn remove_member(
    tx: &mut Transaction<'_, Sqlite>,
    project: &str,
    member: &str,
) -> Result<()> {
    // Bump the item revision before foreign keys clear the assignment and remove access.
    sqlx::query("UPDATE project_items SET revision=revision+1 WHERE project=? AND assignee=?")
        .bind(project)
        .bind(member)
        .execute(&mut **tx)
        .await?;
    let removed =
        sqlx::query("DELETE FROM project_members WHERE project=? AND id=? AND role<>'owner'")
            .bind(project)
            .bind(member)
            .execute(&mut **tx)
            .await?;
    ensure!(removed.rows_affected() == 1, ProjectError::Unavailable);
    Ok(())
}

pub(crate) async fn task_result(
    db: &SqlitePool,
    user: &str,
    project: &str,
    task: &str,
) -> Result<TaskResult> {
    let mut tx = db.begin().await?;
    let actor = joined(&mut tx, user, project).await?;
    ensure!(actor.role.can_publish(), ProjectError::Forbidden);
    let result = read_result(&mut tx, user, task).await?;
    tx.commit().await?;
    Ok(result)
}

async fn read_result(
    tx: &mut Transaction<'_, Sqlite>,
    user: &str,
    task: &str,
) -> Result<TaskResult> {
    let mut result: TaskResult = sqlx::query_as(
        "SELECT t.id AS task,t.title,t.revision,e.id AS event,e.body,0 AS truncated
         FROM tasks t JOIN events e ON e.task=t.id
         WHERE t.id=? AND t.user=? AND t.chief_chat=1 AND t.agent IS NULL
         AND t.shared_memory_pair IS NULL AND t.state IN ('idle','completed')
         AND e.role='assistant' AND e.body<>''
         AND EXISTS(SELECT 1 FROM users u WHERE u.id=t.user AND u.suspended_at IS NULL)
         ORDER BY e.id DESC LIMIT 1",
    )
    .bind(task)
    .bind(user)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(ProjectError::Unavailable)?;
    let clipped = clip(&result.body, 8_000);
    result.truncated = clipped.len() < result.body.len();
    result.body = clipped.to_owned();
    Ok(result)
}

pub(crate) async fn publish(
    db: &SqlitePool,
    user: &str,
    project: &str,
    review: Review,
    source: Source,
) -> Result<String> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let actor = joined(&mut tx, user, project).await?;
    ensure!(actor.role.can_publish(), ProjectError::Forbidden);
    let audience: Vec<String> =
        sqlx::query_scalar("SELECT id FROM active_project_members WHERE project=? ORDER BY id")
            .bind(project)
            .fetch_all(&mut *tx)
            .await?;
    ensure!(
        actor.revision == review.revision && audience == review.audience,
        ProjectError::Changed
    );
    let (title, body, kind, source_revision) = match source {
        Source::Note { title, body } => {
            let title = title.trim();
            let body = body.trim();
            ensure!(
                !title.is_empty() && title.len() <= 120 && !body.is_empty() && body.len() <= 8_000,
                "invalid project note"
            );
            (title.to_owned(), body.to_owned(), "note", None)
        }
        Source::Task {
            task,
            revision,
            event,
        } => {
            let result = read_result(&mut tx, user, &task).await?;
            ensure!(
                result.revision == revision && result.event == event,
                ProjectError::Changed
            );
            (result.title, result.body, "task", Some(revision))
        }
    };
    let item = id();
    sqlx::query(
        "INSERT INTO project_items(id,project,author,title,body,kind,source_revision,created_at)
         VALUES(?,?,?,?,?,?,?,?)",
    )
    .bind(&item)
    .bind(project)
    .bind(user)
    .bind(title)
    .bind(body)
    .bind(kind)
    .bind(source_revision)
    .bind(now())
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO project_item_audience(item,member)
         SELECT ?,id FROM active_project_members WHERE project=?",
    )
    .bind(&item)
    .bind(project)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(item)
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ItemChange {
    Assign { member: Option<String> },
    Check { done: bool },
    Withdraw,
}

pub(crate) async fn update_item(
    db: &SqlitePool,
    user: &str,
    project: &str,
    item: &str,
    revision: i64,
    change: ItemChange,
) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let actor = joined(&mut tx, user, project).await?;
    let (author, assignee, current): (String, Option<String>, i64) = sqlx::query_as(
        "SELECT i.author,i.assignee,i.revision FROM project_items i
         JOIN project_item_audience allowed ON allowed.item=i.id
         WHERE i.project=? AND i.id=? AND allowed.member=?",
    )
    .bind(project)
    .bind(item)
    .bind(&actor.membership)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ProjectError::Unavailable)?;
    ensure!(revision == current, ProjectError::Changed);
    let manages = actor.role == Role::Owner || (actor.role == Role::Editor && author == user);
    match change {
        ItemChange::Assign { member } => {
            ensure!(manages, ProjectError::Forbidden);
            if member == assignee {
                return Ok(());
            }
            let recipient: Option<String> = if let Some(member) = &member {
                Some(
                    sqlx::query_scalar(
                        "SELECT a.user FROM active_project_members a
                     JOIN project_item_audience allowed ON allowed.member=a.id
                     WHERE a.project=? AND a.id=? AND allowed.item=?",
                    )
                    .bind(project)
                    .bind(member)
                    .bind(item)
                    .fetch_optional(&mut *tx)
                    .await?
                    .ok_or(ProjectError::Unavailable)?,
                )
            } else {
                None
            };
            sqlx::query(
                "UPDATE project_items SET assignee=?,done=0,revision=revision+1 WHERE id=?",
            )
            .bind(member)
            .bind(item)
            .execute(&mut *tx)
            .await?;
            if let Some(recipient) = recipient.filter(|recipient| recipient != user) {
                notice(
                    &mut tx,
                    &recipient,
                    ASSIGNMENT,
                    &format!("{item}/{}", revision + 1),
                    "You have a project assignment. Open Memory → Project spaces to review it.",
                )
                .await?;
            }
        }
        ItemChange::Check { done } => {
            ensure!(
                manages || assignee.as_deref() == Some(&actor.membership),
                ProjectError::Forbidden
            );
            sqlx::query("UPDATE project_items SET done=?,revision=revision+1 WHERE id=?")
                .bind(done)
                .bind(item)
                .execute(&mut *tx)
                .await?;
        }
        ItemChange::Withdraw => {
            ensure!(
                actor.role == Role::Owner || author == user,
                ProjectError::Forbidden
            );
            sqlx::query("DELETE FROM project_items WHERE id=?")
                .bind(item)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

async fn notice(
    tx: &mut Transaction<'_, Sqlite>,
    user: &str,
    prefix: &str,
    reference: &str,
    body: &str,
) -> Result<()> {
    sqlx::query("INSERT INTO outbox(user,body,button) VALUES(?,?,?)")
        .bind(user)
        .bind(body)
        .bind(format!("{prefix}{reference}"))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub(crate) async fn notice_allowed(
    db: &SqlitePool,
    user: &str,
    button: &str,
) -> Result<Option<bool>> {
    if let Some(member) = button.strip_prefix(INVITATION) {
        let pending: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM project_members m JOIN project_spaces p ON p.id=m.project
             JOIN users owner ON owner.id=p.owner AND owner.suspended_at IS NULL
             JOIN users u ON u.id=m.user AND u.suspended_at IS NULL
             WHERE m.id=? AND m.user=? AND m.joined_at IS NULL)",
        )
        .bind(member)
        .bind(user)
        .fetch_one(db)
        .await?;
        return Ok(Some(pending));
    }
    if let Some(reference) = button.strip_prefix(ASSIGNMENT) {
        let Some((item, revision)) = reference.split_once('/') else {
            return Ok(Some(false));
        };
        let Ok(revision) = revision.parse::<i64>() else {
            return Ok(Some(false));
        };
        let assigned: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM project_items i
             JOIN active_project_members a ON a.id=i.assignee
             JOIN project_item_audience allowed ON allowed.item=i.id AND allowed.member=a.id
             WHERE i.id=? AND i.revision=? AND i.done=0 AND a.user=?)",
        )
        .bind(item)
        .bind(revision)
        .bind(user)
        .fetch_one(db)
        .await?;
        return Ok(Some(assigned));
    }
    Ok(None)
}
