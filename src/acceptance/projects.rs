use super::*;
use crate::store::projects;
use reqwest::StatusCode;

struct ProjectsApi {
    client: reqwest::Client,
    endpoint: String,
    origin: String,
    inits: [String; 4],
}

impl ProjectsApi {
    async fn send(&self, user: usize, action: &str, data: Value) -> reqwest::Response {
        self.client
            .post(&self.endpoint)
            .header("origin", &self.origin)
            .json(&json!({"init":self.inits[user-1],"action":action,"data":data}))
            .send()
            .await
            .unwrap()
    }

    async fn call(&self, user: usize, action: &str, data: Value) -> Value {
        let response = self.send(user, action, data).await;
        let status = response.status();
        let data = response.json::<Value>().await.unwrap();
        assert!(status.is_success(), "{action} {status}: {data}");
        data
    }

    async fn member(&self, user: usize, project: &Value, change: Value) -> Value {
        self.call(
            user,
            "project-member",
            json!({"project":project,"change":change}),
        )
        .await
    }

    async fn join(&self, user: usize, project: &Value) -> Value {
        let list = self.call(user, "list-projects", json!({})).await;
        let pending = &list["projects"][0];
        assert_eq!(pending["id"], *project);
        assert_eq!(pending["joined"], false);
        let accept = json!({"accept":{"member":pending["membership"],"role":pending["role"]}});
        self.member(user, project, accept).await;
        pending["membership"].clone()
    }

    async fn detail(&self, user: usize, project: &Value) -> Value {
        self.call(user, "project-detail", json!({"project":project}))
            .await
    }
}

async fn fixture_api(db: &SqlitePool) -> ProjectsApi {
    for user in ["3", "4"] {
        store::seed(db, user).await.unwrap();
    }
    let app = Arc::new(app_with_model(db, Arc::new(WidgetChief)));
    let inits = std::array::from_fn(|i| signed_mini_init(&app, (i + 1) as u64));
    let origin = app.origin.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/api/mini", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, web::router(app)).await.unwrap() });
    ProjectsApi {
        client: reqwest::Client::new(),
        endpoint,
        origin,
        inits,
    }
}

fn review(detail: &Value) -> Value {
    let audience: Vec<&Value> = detail["members"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|member| member["active"] == true)
        .map(|member| &member["id"])
        .collect();
    json!({"revision":detail["space"]["revision"],"audience":audience})
}

#[tokio::test]
async fn project_membership_roles_and_snapshot_audiences_are_enforced() {
    let db = fixture().await;
    let api = fixture_api(&db).await;
    let project = &api
        .call(1, "create-project", json!({"title":"Release project"}))
        .await["project"];
    let denied = api
        .send(2, "project-detail", json!({"project":project}))
        .await;
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    api.member(1, project, json!({"invite":{"user":"2","role":"viewer"}}))
        .await;
    let denied = api
        .send(2, "project-detail", json!({"project":project}))
        .await;
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    let stale = review(&api.detail(1, project).await);
    let member = api.join(2, project).await;
    let note = json!({"kind":"note","title":"Release notes","body":"Reviewed progress for this audience."});
    let conflict = api
        .send(
            1,
            "publish-project-item",
            json!({"project":project,"review":stale,"source":note}),
        )
        .await;
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    let current = review(&api.detail(1, project).await);
    let denied = api
        .send(
            2,
            "publish-project-item",
            json!({"project":project,"review":current,"source":note}),
        )
        .await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let item = &api
        .call(
            1,
            "publish-project-item",
            json!({"project":project,"review":current,"source":note}),
        )
        .await["item"];
    assert_eq!(
        api.detail(2, project).await["items"][0]["body"],
        note["body"]
    );

    api.member(1, project, json!({"invite":{"user":"3","role":"editor"}}))
        .await;
    let newcomer = api.join(3, project).await;
    assert_eq!(api.detail(3, project).await["items"], json!([]));
    let outside = api
        .send(
            1,
            "project-item",
            json!({"project":project,"item":item,"revision":0,
        "change":{"assign":{"member":newcomer}}}),
        )
        .await;
    assert_eq!(outside.status(), StatusCode::NOT_FOUND);
    api.call(
        1,
        "project-item",
        json!({"project":project,"item":item,"revision":0,
        "change":{"assign":{"member":member}}}),
    )
    .await;
    let button: String =
        sqlx::query_scalar("SELECT button FROM outbox WHERE button LIKE 'project-assignment/%'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        projects::notice_allowed(&db, "2", &button).await.unwrap(),
        Some(true)
    );
    assert_eq!(
        projects::notice_allowed(&db, "3", &button).await.unwrap(),
        Some(false)
    );
    api.call(
        2,
        "project-item",
        json!({"project":project,"item":item,"revision":1,"change":{"check":{"done":true}}}),
    )
    .await;
    assert_eq!(api.detail(1, project).await["items"][0]["done"], true);
    assert_eq!(
        projects::notice_allowed(&db, "2", &button).await.unwrap(),
        Some(false)
    );

    api.call(
        1,
        "project-item",
        json!({"project":project,"item":item,"revision":2,
        "change":{"assign":{"member":null}}}),
    )
    .await;
    api.call(
        1,
        "project-item",
        json!({"project":project,"item":item,"revision":3,
        "change":{"assign":{"member":member}}}),
    )
    .await;
    let queued: String = sqlx::query_scalar(
        "SELECT button FROM outbox WHERE button LIKE 'project-assignment/%' ORDER BY id DESC",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    api.member(1, project, json!({"revoke":{"member":member}}))
        .await;
    assert_eq!(
        projects::notice_allowed(&db, "2", &queued).await.unwrap(),
        Some(false)
    );
    assert_eq!(
        api.detail(1, project).await["items"][0]["assignee"],
        Value::Null
    );

    api.member(1, project, json!({"invite":{"user":"2","role":"viewer"}}))
        .await;
    let stale_invite = api
        .send(
            2,
            "project-member",
            json!({"project":project,
        "change":{"accept":{"member":member,"role":"viewer"}}}),
        )
        .await;
    assert_eq!(stale_invite.status(), StatusCode::CONFLICT);
    assert_ne!(api.join(2, project).await, member);
    assert_eq!(api.detail(2, project).await["items"], json!([]));
    assert_eq!(
        api.call(4, "list-projects", json!({})).await["projects"],
        json!([])
    );
}

#[tokio::test]
async fn project_task_publication_copies_only_the_reviewed_private_result() {
    let db = fixture().await;
    let api = fixture_api(&db).await;
    let project = &api
        .call(1, "create-project", json!({"title":"Shared workspace"}))
        .await["project"];
    api.member(1, project, json!({"invite":{"user":"2","role":"editor"}}))
        .await;
    api.join(2, project).await;
    message(&db, 10, "2", "Fixture title").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    store::settle_reply(&db, &task, "Shareable fixture result.", &[])
        .await
        .unwrap();
    sqlx::query("INSERT INTO events(task,role,body) VALUES(?,'user','Private fixture follow-up')")
        .bind(&task.id)
        .execute(&db)
        .await
        .unwrap();

    let denied = api
        .send(
            1,
            "preview-project-result",
            json!({"project":project,"task":task.id}),
        )
        .await;
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    let preview = api
        .call(
            2,
            "preview-project-result",
            json!({"project":project,"task":task.id}),
        )
        .await;
    assert_eq!(preview["body"], "Shareable fixture result.");
    assert!(!preview.to_string().contains("Private fixture follow-up"));
    let current = review(&api.detail(2, project).await);
    let source = json!({"kind":"task","task":task.id,"revision":preview["revision"],"event":preview["event"]});

    sqlx::query(
        "INSERT INTO events(task,role,body) VALUES(?,'assistant','Updated fixture result.')",
    )
    .bind(&task.id)
    .execute(&db)
    .await
    .unwrap();
    let stale = api
        .send(
            2,
            "publish-project-item",
            json!({"project":project,"review":current,"source":source}),
        )
        .await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    let preview = api
        .call(
            2,
            "preview-project-result",
            json!({"project":project,"task":task.id}),
        )
        .await;
    let source = json!({"kind":"task","task":task.id,"revision":preview["revision"],"event":preview["event"]});
    api.call(
        2,
        "publish-project-item",
        json!({"project":project,"review":current,"source":source}),
    )
    .await;

    sqlx::query("UPDATE events SET body='Changed original' WHERE id=?")
        .bind(preview["event"].as_i64())
        .execute(&db)
        .await
        .unwrap();
    let owner = api.detail(1, project).await;
    assert_eq!(owner["items"][0]["body"], "Updated fixture result.");
    assert_eq!(owner["items"][0]["kind"], "task");
    assert!(!owner.to_string().contains(&task.id));
}
