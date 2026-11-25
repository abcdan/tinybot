use super::*;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use std::sync::Mutex;
use tower_service::Service;

const WORKSPACE: &str = "f1330000-0000-4000-8000-000000000002";
const NOTE: &str = "f1330000-0000-4000-8000-000000000001";
const URL: &str = "https://docblock.dev/personal#/note/f1330000-0000-4000-8000-000000000001";
const RESULT: &str = "# Project review\nTwo decisions were confirmed.";

async fn mini(
    router: &mut Router,
    app: &App,
    user: u64,
    action: &str,
    data: Value,
) -> (StatusCode, Value) {
    let response = router
        .call(
            Request::post("/api/mini")
                .header("origin", &app.origin)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"init":signed_mini_init(app,user),"action":action,"data":data})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 100_000).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn task_result_save_reviews_exact_note_and_writes_the_approved_draft_once() {
    let db = fixture().await;
    let mut app = app_with_model(&db, Arc::new(WidgetChief));
    let connector = app.mcp.prepare_docblock("1").await.unwrap();
    let credential = json!({
        "flow":{"connector":connector.id,"client_id":"test","client_secret":null,
        "token_url":"https://docblock.dev/oauth/token","verifier":"test","resource":connector.endpoint},
        "scopes":["notes:read","notes:write"],"access_token":"synthetic","refresh_token":null,"expires":now()+600
    });
    let credential = app
        .vault
        .seal(
            &format!("connector:1:{}", connector.id),
            credential.to_string().as_bytes(),
        )
        .unwrap();
    sqlx::query("UPDATE connectors SET auth='oauth',credential=? WHERE id=?")
        .bind(credential)
        .bind(&connector.id)
        .execute(&db)
        .await
        .unwrap();
    let writes = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&writes);
    app.mcp.set_docblock_fixture(move |tool, arguments| {
        let output = match tool {
            "list_workspaces" => json!({"workspaces":[{"id":WORKSPACE,"name":"Personal","role":"owner"}]}),
            "list_notes" => {
                assert_eq!(arguments["workspace"], WORKSPACE);
                json!({"folders":["projects/team","../invalid","files/private"]})
            }
            "read_note" => return Ok(json!({"isError":true,"content":[{"type":"text","text":"not_found: Note not found"}]})),
            "write_note" => {
                captured.lock().unwrap().push(arguments.clone());
                json!({"applied":true,"note":{"id":NOTE,"workspaceId":WORKSPACE,"path":arguments["path"],"version":1,"url":URL}})
            }
            other => panic!("unexpected Docblock tool {other}"),
        };
        Ok(json!({"content":[{"type":"text","text":output.to_string()}]}))
    });

    store::ingest_telegram(&db, "1", 1, "1", "Summarize the project", 10)
        .await
        .unwrap();
    let source = store::claim(&db).await.unwrap().unwrap();
    store::settle_reply(&db, &source, RESULT, &[])
        .await
        .unwrap();
    assert!(
        store::task_views::complete(&db, "1", &source.id, source.revision)
            .await
            .unwrap()
    );
    let app = Arc::new(app);
    let mut router = web::router(app.clone());
    let detail_query = json!({"task":source.id});
    let (_, source_view) = mini(&mut router, &app, 1, "task-detail", detail_query.clone()).await;
    assert_eq!(source_view["task"]["state"], "completed");
    assert_eq!(source_view["docblock_saves"], json!([]));
    let folder_query = json!({"id":connector.id,"workspace":WORKSPACE});
    let (status, folders) = mini(&mut router, &app, 1, "docblock-folders", folder_query).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        folders["folders"],
        json!(["projects/team", "files/private"])
    );

    let mut save = json!({"task":source.id,"revision":source_view["task"]["revision"],
        "event":source_view["reply"]["event"],"id":connector.id,"workspace":WORKSPACE,
        "folder":"projects/team","title":"Project review"});
    assert_eq!(
        mini(
            &mut router,
            &app,
            2,
            "save-result-to-docblock",
            save.clone()
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (status, review) = mini(
        &mut router,
        &app,
        1,
        "save-result-to-docblock",
        save.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(review["status"], "review");
    assert_eq!(
        mini(
            &mut router,
            &app,
            1,
            "save-result-to-docblock",
            save.clone()
        )
        .await
        .1,
        review
    );
    assert!(writes.lock().unwrap().is_empty());

    let approval = json!({"task":review["approval_task"]});
    let (_, details) = mini(&mut router, &app, 1, "task-detail", approval).await;
    let preview = &details["requests"][0]["content"]["docblock"];
    assert_eq!(preview["workspace"], WORKSPACE);
    assert_eq!(preview["target"], review["path"]);
    assert_eq!(
        preview["change"],
        format!("Title: Project review\n{RESULT}")
    );
    let consent = json!({"id":review["request"]});
    assert_eq!(
        mini(&mut router, &app, 1, "always-approve-mcp", consent.clone())
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        mini(&mut router, &app, 2, "approve", consent.clone())
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        mini(&mut router, &app, 1, "approve", consent.clone())
            .await
            .0,
        StatusCode::OK
    );
    let work = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(work.id, review["approval_task"]);
    runtime::work(&app, &work).await.unwrap();
    {
        let writes = writes.lock().unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0]["body"], RESULT);
        assert_eq!(writes[0]["path"], review["path"]);
    }
    let (_, result) = mini(&mut router, &app, 1, "task-detail", detail_query.clone()).await;
    assert_eq!(result["task"]["state"], "completed");
    assert_eq!(result["docblock_saves"][0]["status"], "saved");
    assert_eq!(result["docblock_saves"][0]["url"], URL);
    assert_eq!(
        mini(
            &mut router,
            &app,
            1,
            "save-result-to-docblock",
            save.clone()
        )
        .await
        .1["status"],
        "saved"
    );
    assert_eq!(
        mini(&mut router, &app, 1, "approve", consent).await.0,
        StatusCode::BAD_REQUEST
    );
    assert!(store::claim(&db).await.unwrap().is_none());

    save["title"] = json!("Another note");
    let (_, second) = mini(
        &mut router,
        &app,
        1,
        "save-result-to-docblock",
        save.clone(),
    )
    .await;
    assert_eq!(
        mini(
            &mut router,
            &app,
            1,
            "approve",
            json!({"id":second["request"]})
        )
        .await
        .0,
        StatusCode::OK
    );
    sqlx::query("UPDATE tasks SET revision=revision+1 WHERE id=?")
        .bind(&source.id)
        .execute(&db)
        .await
        .unwrap();
    let work = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &work).await.unwrap();
    assert_eq!(writes.lock().unwrap().len(), 1);
    let (_, result) = mini(&mut router, &app, 1, "task-detail", detail_query).await;
    assert!(
        result["docblock_saves"]
            .as_array()
            .unwrap()
            .iter()
            .any(|save| save["request"] == second["request"] && save["status"] == "failed")
    );
}
