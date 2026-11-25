use super::*;
use crate::store::invitations::{self, Review};

async fn invite(db: &SqlitePool, guest: &str, expanded: bool) {
    let mut tx = db.begin().await.unwrap();
    let invitation = invitations::create(&mut tx, "1").await.unwrap();
    tx.commit().await.unwrap();
    let Review::Available(id) = invitations::review(
        db,
        "1",
        guest,
        invitation.command.strip_prefix("/start ").unwrap(),
    )
    .await
    .unwrap() else {
        panic!("expected a valid invitation");
    };
    assert!(invitations::accept(db, guest, &id, expanded).await.unwrap());
}

#[tokio::test]
async fn guest_usage_and_spend_alerts_stay_scoped_to_the_signed_account() {
    let db = fixture().await;
    invite(&db, "3", true).await;
    invite(&db, "4", false).await;
    sqlx::query("UPDATE users SET provider='codex' WHERE id IN ('3','4')")
        .execute(&db)
        .await
        .unwrap();
    let app = Arc::new(app_with_model(&db, Arc::new(WidgetChief)));
    let operator = signed_mini_init(&app, 1);
    let guest = signed_mini_init(&app, 3);
    let legacy = signed_mini_init(&app, 4);
    let origin = app.origin.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/api/mini", listener.local_addr().unwrap());
    let server =
        tokio::spawn(async move { axum::serve(listener, web::router(app)).await.unwrap() });
    let client = reqwest::Client::new();
    let post = |init: &str, action: &str, data: Value| {
        client
            .post(&endpoint)
            .header("origin", &origin)
            .json(&json!({"init":init,"action":action,"data":data}))
            .send()
    };

    let response = post(&guest, "list-invites", json!({})).await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    let response = post(
        &guest,
        "save-usage-alert",
        json!({"user":"4","weekly_cents":10}),
    )
    .await
    .unwrap();
    assert!(response.status().is_success());
    let response = post(&guest, "save-usage-alert", json!({"weekly_cents":-1}))
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);

    message(&db, 1, "3", "Private invitation fixture").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    let trace = audit::Trace::start(&db, &task).await.unwrap();
    audit::scope(trace, async {
        for report in [
            json!({"usage":{"total_tokens":42}}),
            json!({"usage":{"total_tokens":10,"cost":0.125}}),
            json!({"usage":{"cost":0.05}}),
        ] {
            audit::provider_call("fixture", "fixture/model", async |call| {
                call.openrouter(&report);
                Ok(())
            })
            .await
            .unwrap();
        }
    })
    .await;
    let notices: Vec<(String, String)> = sqlx::query_as(
        "SELECT user,body FROM outbox WHERE body LIKE 'TinyBot reached your weekly spend alert%'",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].0, "3");
    assert!(notices[0].1.contains("1 of 2"));
    store::settle_reply(&db, &task, "Fixture result.", &[])
        .await
        .unwrap();
    sqlx::query("UPDATE outbox SET state='sent' WHERE assistant_event_id IS NOT NULL")
        .execute(&db)
        .await
        .unwrap();
    assert!(
        store::task_views::complete(&db, "3", &task.id, task.revision)
            .await
            .unwrap()
    );

    let mine: Value = post(&guest, "my-usage", json!({"user":"4"}))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mine["alert"]["weekly_cents"], 10);
    assert_eq!(mine["alert"]["spent_microusd"], 175_000);
    assert_eq!(mine["alert"]["cost_reported_calls"], 2);
    assert!(mine["guest"]["first_result_at_ms"].is_number());
    assert_eq!(mine["delivery"]["delivered_runs"], 1);

    let other: Value = post(&legacy, "my-usage", json!({"user":"3"}))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(other["alert"]["weekly_cents"], Value::Null);
    assert_eq!(other["usage"]["calls"], 0);
    assert_eq!(other["guest"]["first_result_at_ms"], Value::Null);

    let roster: Value = post(&operator, "list-invites", json!({}))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let details = |uid: &str| {
        roster["guests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["id"] == uid)
            .unwrap()
    };
    assert_eq!(details("3")["first_result"], true);
    assert_eq!(details("3")["delivered_runs"], 1);
    assert_eq!(details("3")["checked_off_tasks"], 1);
    assert_eq!(details("3")["cost_reported_calls"], 2);
    assert_eq!(details("4")["first_result"], Value::Null);
    assert_eq!(details("4")["delivered_runs"], Value::Null);
    let retained = roster.to_string();
    assert!(
        !retained.contains("Private invitation fixture")
            && !retained.contains("first_result_at_ms")
    );
    assert!(!retained.contains("weekly_cents"));
    server.abort();
}
