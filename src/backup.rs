use anyhow::{Result, ensure};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::path::Path;

pub(crate) async fn check(path: &Path) -> Result<()> {
    ensure!(path.is_file(), "SQLite snapshot file unavailable");
    let db = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().filename(path).read_only(true))
        .await?;
    let schema: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('users','tasks','events','_sqlx_migrations')",
    ).fetch_one(&db).await?;
    ensure!(schema == 4, "snapshot does not contain a TinyBot database");
    let integrity: Vec<String> = sqlx::query_scalar("PRAGMA quick_check")
        .fetch_all(&db)
        .await?;
    ensure!(
        integrity == ["ok"],
        "SQLite snapshot integrity check failed"
    );
    ensure!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_optional(&db)
            .await?
            .is_none(),
        "SQLite snapshot contains invalid references"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_snapshot_opens_read_only_and_retains_synthetic_task_state() {
        let directory =
            std::env::temp_dir().join(format!("tinybot-restore-fixture-{}", crate::security::id()));
        std::fs::create_dir(&directory).unwrap();
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(directory.join("source.sqlite"))
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        sqlx::migrate!().run(&db).await.unwrap();
        crate::store::seed(&db, "1").await.unwrap();
        crate::store::ingest_telegram(&db, "1", 1, "1", "Synthetic restore drill", 7)
            .await
            .unwrap();
        let path = directory.join("snapshot.sqlite");
        sqlx::query("VACUUM INTO ?")
            .bind(path.to_str().unwrap())
            .execute(&db)
            .await
            .unwrap();
        check(&path).await.unwrap();
        let restored = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(&path).read_only(true))
            .await
            .unwrap();
        let message: String = sqlx::query_scalar("SELECT body FROM events WHERE role='user'")
            .fetch_one(&restored)
            .await
            .unwrap();
        assert_eq!(message, "Synthetic restore drill");
        restored.close().await;
        db.close().await;
        std::fs::remove_dir_all(directory).unwrap();
    }
}
