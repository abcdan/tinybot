mod ingress;
mod queue;
mod requests;

pub use ingress::ingest;
pub use queue::{
    append, claim, compact, compaction_batch, current, expire, has_access, history, recover, settle,
};
pub use requests::{
    can_deliver, claim_approval, claim_outbox, finish_action, notify, pause, respond, start_action,
    uncertain_action,
};

use crate::security::{id, now};
use anyhow::Result;
use serde::Serialize;
use sqlx::{
    FromRow, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{path::Path, time::Duration};

#[derive(Clone, FromRow, Serialize)]
pub struct Task {
    pub id: String,
    pub user: String,
    pub agent: Option<String>,
    pub title: String,
    pub state: String,
    pub revision: i64,
    pub summary: String,
    pub goal: String,
}

#[derive(Clone, FromRow, Serialize)]
pub struct Agent {
    pub id: String,
    pub owner: String,
    pub name: String,
    pub instructions: String,
    pub tools: String,
}

#[derive(Clone, FromRow)]
pub struct Pending {
    pub id: String,
    pub task: Option<String>,
    pub revision: Option<i64>,
    pub kind: String,
    pub payload: String,
    pub expires: i64,
}

#[derive(FromRow)]
pub struct Outbound {
    pub id: i64,
    pub user: String,
    pub agent: Option<String>,
    pub task: Option<String>,
    pub body: String,
    pub button: Option<String>,
}

pub async fn open(path: &Path) -> Result<SqlitePool> {
    let db = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal)
                .foreign_keys(true)
                .busy_timeout(Duration::from_secs(8))
                .pragma("secure_delete", "ON")
                .pragma("temp_store", "MEMORY"),
        )
        .await?;
    sqlx::migrate!().run(&db).await?;
    Ok(db)
}

pub async fn seed(db: &SqlitePool, operator: &str) -> Result<()> {
    sqlx::query("INSERT OR IGNORE INTO users(id,host_id,onboarded_at) VALUES(?,?,?)")
        .bind(operator)
        .bind(format!("urn:uuid:{}", id()))
        .bind(now())
        .execute(db)
        .await?;
    Ok(())
}
