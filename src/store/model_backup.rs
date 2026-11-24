use super::Task;
use crate::model::BackupModel;
use anyhow::Result;
use sqlx::SqlitePool;

pub(crate) async fn selected(db: &SqlitePool, task: &Task) -> Result<Option<BackupModel>> {
    Ok(sqlx::query_as(
        "SELECT b.provider,b.model FROM task_model_backups b JOIN tasks t ON t.id=b.task AND t.revision=b.revision
         JOIN users u ON u.id=t.user
         WHERE t.id=? AND t.user=? AND t.revision=? AND t.state='running'
           AND u.suspended_at IS NULL AND u.provider<>b.provider
           AND u.backup_provider=b.provider AND u.backup_model=b.model",
    ).bind(&task.id).bind(&task.user).bind(task.revision).fetch_optional(db).await?)
}

pub(crate) async fn begin(db: &SqlitePool, task: &Task) -> Result<Option<BackupModel>> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let backup: Option<BackupModel> = sqlx::query_as(
        "INSERT INTO task_model_backups(task,revision,provider,model)
         SELECT t.id,t.revision,u.backup_provider,u.backup_model FROM tasks t JOIN users u ON u.id=t.user
         WHERE t.id=? AND t.user=? AND t.revision=? AND t.agent IS NULL AND t.chief_chat=1
           AND t.shared_memory_pair IS NULL AND t.state='running' AND u.suspended_at IS NULL
           AND u.provider<>u.backup_provider AND u.backup_model IS NOT NULL
           AND ((u.backup_provider='codex' AND u.codex_credential IS NOT NULL AND u.codex_model=u.backup_model)
                OR (u.backup_provider='openrouter' AND u.openrouter_key IS NOT NULL))
           AND NOT EXISTS(SELECT 1 FROM task_tool_attempts a WHERE a.task=t.id AND a.revision=t.revision)
           AND NOT EXISTS(SELECT 1 FROM pending p WHERE p.task=t.id AND p.revision=t.revision)
         ON CONFLICT(task,revision) DO NOTHING RETURNING provider,model",
    ).bind(&task.id).bind(&task.user).bind(task.revision).fetch_optional(&mut *tx).await?;
    if let Some(backup) = &backup {
        let message = format!(
            "Your task model is temporarily unavailable. I’ll try your backup, {} · {}.",
            backup.provider_label(),
            backup.model
        );
        sqlx::query("INSERT INTO outbox(user,task,body) VALUES(?,?,?)")
            .bind(&task.user)
            .bind(&task.id)
            .bind(message)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(backup)
}
