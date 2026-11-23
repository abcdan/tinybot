use super::*;
use crate::store;
use serde_json::json;
use sqlx::sqlite::SqlitePoolOptions;

async fn fixture() -> SqlitePool {
    let db = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!().run(&db).await.unwrap();
    for user in ["1", "2"] {
        store::seed(&db, user).await.unwrap();
    }
    db
}

async fn running_task(db: &SqlitePool, user: &str, agent: Option<&str>) -> Task {
    sqlx::query_as("INSERT INTO tasks(id,user,agent,title,updated,state) VALUES(?,?,?,'Task',0,'running') RETURNING *")
        .bind(crate::security::id())
        .bind(user)
        .bind(agent)
        .fetch_one(db)
        .await
        .unwrap()
}

#[tokio::test]
async fn chief_and_helpers_share_exact_checkpoints_and_a_hard_cap() {
    let db = fixture().await;
    let task = running_task(&db, "1", None).await;
    let args = [
        serde_json::from_str(r#"{"key":"private-value","nested":{"a":1,"b":2}}"#).unwrap(),
        serde_json::from_str(r#"{"nested":{"b":2,"a":1},"key":"private-value"}"#).unwrap(),
    ];
    for position in 1..=LIMIT {
        let index = (position % 2) as usize;
        let actor = ["chief", "helper"][index];
        assert!(
            record(&db, &task, actor, "call_mcp", &args[index])
                .await
                .unwrap()
        );
        if position % INTERVAL != 0 {
            assert!(
                progress(&db, &task)
                    .await
                    .unwrap()
                    .unwrap()
                    .pending
                    .is_none()
            );
            continue;
        }
        let current = progress(&db, &task).await.unwrap().unwrap();
        assert_eq!(current.calls, position);
        assert!(!current.stalled);
        let batch = current.pending.unwrap();
        assert_eq!(batch.position, position);
        assert_eq!(batch.attempts.len(), INTERVAL as usize);
        assert_eq!(batch.attempts[0].position, position - INTERVAL + 1);
        assert_eq!(batch.attempts[0].actor, "helper");
        assert_eq!(batch.attempts[1].actor, "chief");
        assert!(batch.attempts.iter().all(|attempt| {
            attempt.tool == "call_mcp"
                && attempt.args_fingerprint.len() == 64
                && attempt.args_fingerprint == batch.attempts[0].args_fingerprint
        }));
        assert!(
            !record(&db, &task, "helper", "call_mcp", &args[0])
                .await
                .unwrap()
        );
        assert!(complete_check(&db, &task, position, false).await.unwrap());
        assert!(!complete_check(&db, &task, position, true).await.unwrap());
    }
    assert!(
        !record(&db, &task, "chief", "send_reply", &json!({}))
            .await
            .unwrap()
    );
    assert_eq!(progress(&db, &task).await.unwrap().unwrap().calls, 100);
}

#[tokio::test]
async fn checkpoint_survives_waiting_and_stall_blocks_until_next_revision() {
    let db = fixture().await;
    sqlx::query("INSERT INTO agents VALUES('shared','1','Shared','Help','[]')")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO grants VALUES('shared','2')")
        .execute(&db)
        .await
        .unwrap();
    let mut task = running_task(&db, "2", Some("shared")).await;
    let mut wrong_user = task.clone();
    wrong_user.user = "1".into();
    assert!(progress(&db, &wrong_user).await.unwrap().is_none());
    assert!(
        !record(&db, &wrong_user, "helper", "read", &json!({}))
            .await
            .unwrap()
    );
    for _ in 0..INTERVAL {
        assert!(
            record(&db, &task, "helper", "read", &json!({}))
                .await
                .unwrap()
        );
    }

    sqlx::query("UPDATE tasks SET state='waiting' WHERE id=?")
        .bind(&task.id)
        .execute(&db)
        .await
        .unwrap();
    assert!(progress(&db, &task).await.unwrap().is_none());
    assert!(!complete_check(&db, &task, INTERVAL, false).await.unwrap());
    sqlx::query("UPDATE tasks SET state='running' WHERE id=?")
        .bind(&task.id)
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        progress(&db, &task)
            .await
            .unwrap()
            .unwrap()
            .pending
            .unwrap()
            .position,
        INTERVAL
    );
    assert!(complete_check(&db, &task, INTERVAL, true).await.unwrap());
    assert!(progress(&db, &task).await.unwrap().unwrap().stalled);
    assert!(
        !record(&db, &task, "chief", "read", &json!({}))
            .await
            .unwrap()
    );

    sqlx::query("UPDATE tasks SET revision=revision+1 WHERE id=?")
        .bind(&task.id)
        .execute(&db)
        .await
        .unwrap();
    assert!(progress(&db, &task).await.unwrap().is_none());
    let remaining: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM task_tool_attempts),(SELECT COUNT(*) FROM task_tool_checks)",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(remaining, (0, 0));
    task.revision += 1;
    assert_eq!(progress(&db, &task).await.unwrap().unwrap().calls, 0);
    assert!(
        record(&db, &task, "helper", "read", &json!({}))
            .await
            .unwrap()
    );

    sqlx::query("DELETE FROM grants WHERE agent='shared' AND member='2'")
        .execute(&db)
        .await
        .unwrap();
    assert!(progress(&db, &task).await.unwrap().is_none());
    assert!(
        !record(&db, &task, "helper", "read", &json!({}))
            .await
            .unwrap()
    );
}
