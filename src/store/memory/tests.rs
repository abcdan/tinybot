use super::*;
use crate::store;
use sqlx::sqlite::SqlitePoolOptions;

async fn database() -> SqlitePool {
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap()
}

async fn fixture() -> SqlitePool {
    let db = database().await;
    sqlx::migrate!().run(&db).await.unwrap();
    for user in ["owner", "guest"] {
        store::seed(&db, user).await.unwrap();
    }
    sqlx::query(
        "INSERT INTO agents VALUES ('one','owner','One','First helper','[]'),
                                   ('two','owner','Two','Second helper','[]')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO grants VALUES ('one','guest')")
        .execute(&db)
        .await
        .unwrap();
    db
}

async fn running(db: &SqlitePool, user: &str, agent: Option<&str>) -> Task {
    sqlx::query_as(
        "INSERT INTO tasks(id,user,agent,title,state,updated)
         VALUES(?,?,?,'Memory task','running',0) RETURNING *",
    )
    .bind(id())
    .bind(user)
    .bind(agent)
    .fetch_one(db)
    .await
    .unwrap()
}

#[tokio::test]
async fn each_owned_role_gets_its_private_memory_and_the_account_group() {
    let db = fixture().await;
    let parent = running(&db, "owner", None).await;
    remember(&db, &parent, None, false, "Chief private")
        .await
        .unwrap();
    remember(&db, &parent, Some("one"), false, "One private")
        .await
        .unwrap();
    remember(&db, &parent, Some("two"), false, "Two private")
        .await
        .unwrap();
    remember(&db, &parent, Some("one"), true, "Group shared")
        .await
        .unwrap();
    sqlx::query("INSERT INTO memory VALUES('published','owner','one','Published note')")
        .execute(&db)
        .await
        .unwrap();

    assert_eq!(
        chief_context(&db, &parent).await.unwrap(),
        ["Group shared", "Chief private"]
    );
    assert_eq!(
        helper_context(&db, &parent, Some("one")).await.unwrap(),
        ["Group shared", "One private", "Published note"]
    );
    assert_eq!(
        helper_context(&db, &parent, Some("two")).await.unwrap(),
        ["Group shared", "Two private"]
    );
    assert!(helper_context(&db, &parent, None).await.unwrap().is_empty());

    let other_chief = running(&db, "guest", None).await;
    assert!(chief_context(&db, &other_chief).await.unwrap().is_empty());
    assert!(
        helper_context(&db, &other_chief, Some("one"))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        remember(&db, &other_chief, Some("one"), true, "Cannot share")
            .await
            .is_err()
    );
    let external = running(&db, "guest", Some("one")).await;
    assert_eq!(
        chief_context(&db, &external).await.unwrap(),
        ["Published note"]
    );
    assert_eq!(
        helper_context(&db, &external, Some("one")).await.unwrap(),
        ["Published note"]
    );
    assert!(
        helper_context(&db, &external, Some("two"))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        recall(&db, &external, Some("one"), "private")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        remember(&db, &external, None, false, "Cannot write")
            .await
            .is_err()
    );
    assert!(
        remember(&db, &external, Some("one"), true, "Cannot share")
            .await
            .is_err()
    );

    sqlx::query("DELETE FROM grants WHERE agent='one' AND member='guest'")
        .execute(&db)
        .await
        .unwrap();
    assert!(chief_context(&db, &external).await.unwrap().is_empty());
    assert!(
        recall(&db, &external, Some("one"), "note")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn saves_are_deduplicated_and_pages_and_search_respect_scope() {
    let db = fixture().await;
    let older = save(
        &db,
        "owner",
        MemoryScope::Group,
        None,
        "Old Dutch preference 100%",
    )
    .await
    .unwrap();
    let duplicate = save(
        &db,
        "owner",
        MemoryScope::Group,
        None,
        " Old Dutch preference 100% ",
    )
    .await
    .unwrap();
    assert_eq!(older.id, duplicate.id);
    assert!(older.created_at > 0);
    for index in 0..30 {
        save(
            &db,
            "owner",
            MemoryScope::Group,
            None,
            &format!("New group note {index}"),
        )
        .await
        .unwrap();
    }
    let (first, cursor) = entries(&db, "owner", MemoryScope::Group, None, None)
        .await
        .unwrap();
    assert_eq!(first.len(), 30);
    assert_eq!(first[0].body, "New group note 29");
    let (second, cursor) = entries(&db, "owner", MemoryScope::Group, None, cursor)
        .await
        .unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].id, older.id);
    assert_eq!(cursor, None);

    let task = running(&db, "owner", None).await;
    assert_eq!(chief_context(&db, &task).await.unwrap().len(), 16);
    assert_eq!(
        recall(&db, &task, None, "DUTCH").await.unwrap(),
        ["Old Dutch preference 100%"]
    );
    assert_eq!(
        recall(&db, &task, None, "%").await.unwrap(),
        ["Old Dutch preference 100%"]
    );
    assert_eq!(recall(&db, &task, None, "note").await.unwrap().len(), 8);
    assert!(recall(&db, &task, None, " ").await.is_err());
    assert!(recall(&db, &task, None, &"q".repeat(161)).await.is_err());
    assert!(
        save(&db, "guest", MemoryScope::Helper, Some("one"), "No access")
            .await
            .is_err()
    );
    assert!(
        save(
            &db,
            "owner",
            MemoryScope::Group,
            Some("one"),
            "Invalid scope"
        )
        .await
        .is_err()
    );
    assert!(!remove(&db, "guest", &older.id).await.unwrap());
    assert!(remove(&db, "owner", &older.id).await.unwrap());
    assert!(!remove(&db, "owner", &older.id).await.unwrap());
}

#[tokio::test]
async fn memory_writes_require_the_persisted_running_revision_and_access() {
    let db = fixture().await;
    let scoped = running(&db, "guest", Some("one")).await;
    let forged = Task {
        agent: None,
        ..scoped.clone()
    };
    assert!(
        remember(&db, &forged, None, true, "Private write from guest scope")
            .await
            .is_err()
    );

    let task = running(&db, "owner", None).await;
    sqlx::query("UPDATE tasks SET revision=revision+1 WHERE id=?")
        .bind(&task.id)
        .execute(&db)
        .await
        .unwrap();
    assert!(
        remember(&db, &task, None, true, "Stale group")
            .await
            .is_err()
    );
    assert!(
        remember(&db, &task, Some("one"), false, "Stale helper")
            .await
            .is_err()
    );
    let (all, _) = entries(&db, "owner", MemoryScope::Group, None, None)
        .await
        .unwrap();
    assert!(all.is_empty());
}

#[tokio::test]
async fn migration_preserves_private_notes_and_the_published_revocation_path() {
    let db = database().await;
    let migrator = sqlx::migrate!();
    for migration in migrator.iter().filter(|migration| migration.version < 14) {
        sqlx::raw_sql(&migration.sql).execute(&db).await.unwrap();
    }
    for user in ["owner", "guest"] {
        store::seed(&db, user).await.unwrap();
    }
    sqlx::query("INSERT INTO agents VALUES ('one','owner','One','First helper','[]')")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO grants VALUES ('one','guest')")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO memory VALUES ('legacy-first','owner',NULL,' Old private '),
                                   ('legacy-duplicate','owner',NULL,'Old private'),
                                   ('published','owner','one','Published note')",
    )
    .execute(&db)
    .await
    .unwrap();
    let guest = running(&db, "guest", Some("one")).await;
    let migration = migrator
        .iter()
        .find(|migration| migration.version == 14)
        .unwrap();
    sqlx::raw_sql(&migration.sql).execute(&db).await.unwrap();

    let (notes, _) = entries(&db, "owner", MemoryScope::Chief, None, None)
        .await
        .unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].id, "legacy-first");
    assert_eq!(notes[0].body, "Old private");
    assert_eq!(notes[0].created_at, 0);
    assert_eq!(
        chief_context(&db, &guest).await.unwrap(),
        ["Published note"]
    );
    sqlx::query("DELETE FROM memory WHERE id='published'")
        .execute(&db)
        .await
        .unwrap();
    assert!(!store::current(&db, &guest).await.unwrap());
}
