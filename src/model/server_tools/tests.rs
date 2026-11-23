use super::*;
use crate::{security::Vault, store};
use sqlx::sqlite::SqlitePoolOptions;

#[test]
fn one_documented_read_only_tool_per_request() {
    let url = Url::parse("https://example.org/docs").unwrap();
    let calls = [
        (
            ServerToolInput::WebSearch("city forecast"),
            "openrouter:web_search",
        ),
        (ServerToolInput::WebFetch(url), "openrouter:web_fetch"),
        (
            ServerToolInput::Datetime(chrono_tz::Europe::Amsterdam),
            "openrouter:datetime",
        ),
        (
            ServerToolInput::SearchModels("image models"),
            "openrouter:experimental__search_models",
        ),
    ];
    for (call, expected) in calls {
        let wire = call.request("openai/gpt-fixture");
        assert_eq!(wire["tools"].as_array().unwrap().len(), 1);
        assert_eq!(wire["tools"][0]["type"], expected);
        assert_eq!(wire["max_tool_calls"], 1);
        assert_eq!(wire["parallel_tool_calls"], false);
        assert_eq!(wire["store"], false);
        if matches!(call.tool(), ServerTool::WebFetch) {
            assert_eq!(
                wire["tools"][0]["parameters"]["allowed_domains"],
                json!(["example.org"])
            );
            assert_eq!(wire["tools"][0]["parameters"]["max_uses"], 1);
        }
    }
}

#[test]
fn only_reported_execution_yields_a_bounded_answer_and_provider_sources() {
    let mut reply = json!({
        "status":"completed",
        "usage":{"input_tokens":110,"output_tokens":40,
                 "server_tool_use_details":{"tool_calls_requested":1,"tool_calls_executed":1,"web_search_requests":1}},
        "output":[{"type":"reasoning","summary":[{"type":"summary_text","text":"raw reasoning"}]},
                  {"type":"message","role":"assistant","content":[{
                      "type":"output_text","text":"The public site reports clear skies.",
                      "annotations":[
                          {"type":"url_citation","url":"https://example.org/forecast","title":"Forecast"},
                          {"type":"url_citation","url":"https://example.org/forecast","title":"Duplicate"},
                          {"type":"url_citation","url":"https://user:fixture@example.org/private","title":"Private"},
                          {"type":"irrelevant","url":"https://example.net","title":"Not a citation"}
                      ]
                  }]}]
    });
    let result = parse_response(ServerTool::WebSearch, &reply).unwrap();
    assert_eq!(result["text"], "The public site reports clear skies.");
    assert_eq!(
        result["citations"],
        json!([{"url":"https://example.org/forecast","title":"Forecast"}])
    );
    assert!(!result.to_string().contains("raw reasoning"));
    assert!(parse_response(ServerTool::Datetime, &reply).is_err());
    for tool in ServerTool::ALL {
        let detail = &mut reply["usage"]["server_tool_use_details"];
        detail["web_search_requests"] = json!(u64::from(tool == ServerTool::WebSearch));
        detail["tool_calls_executed"] = json!(1);
        assert_eq!(parse_response(tool, &reply).unwrap()["tool"], tool.key());
        for executions in [Value::Null, json!(0), json!(2)] {
            reply["usage"]["server_tool_use_details"]["tool_calls_executed"] = executions;
            assert!(parse_response(tool, &reply).is_err());
        }
    }
}

#[tokio::test]
async fn settings_are_default_on_independent_and_only_apply_to_active_openrouter() {
    let db = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!().run(&db).await.unwrap();
    let vault = Vault::new(&"2e".repeat(32)).unwrap();
    let models = Models::new(db.clone(), vault.clone(), "unused".into(), None, None);
    for (user, provider) in [("1", "openrouter"), ("2", "codex")] {
        store::seed(&db, user).await.unwrap();
        let key = vault
            .seal(&format!("openrouter:{user}"), b"sk-or-v1-fixture")
            .unwrap();
        sqlx::query(
            "UPDATE users SET provider=?,model='openai/gpt-fixture',openrouter_key=? WHERE id=?",
        )
        .bind(provider)
        .bind(key)
        .bind(user)
        .execute(&db)
        .await
        .unwrap();
    }
    assert_eq!(
        models.enabled_server_tools("1").await.unwrap(),
        ServerTool::ALL
    );
    assert!(models.enabled_server_tools("2").await.unwrap().is_empty());
    models
        .set_server_tool("1", "web_search", false)
        .await
        .unwrap();
    assert_eq!(
        models.server_tool_settings("1").await.unwrap()["web_search"],
        false
    );
    assert_eq!(
        models.server_tool_settings("1").await.unwrap()["web_fetch"],
        true
    );
    assert_eq!(
        models.server_tool_settings("2").await.unwrap()["web_search"],
        true
    );
    assert!(
        models
            .server_tool_connection("1", ServerTool::WebSearch)
            .await
            .is_err()
    );
    assert!(
        models
            .server_tool_connection("2", ServerTool::WebFetch)
            .await
            .is_err()
    );
    assert!(models.set_server_tool("1", "shell", true).await.is_err());
    models
        .set_server_tool("1", "web_search", true)
        .await
        .unwrap();
    assert!(
        models
            .server_tool_connection("1", ServerTool::WebSearch)
            .await
            .is_ok()
    );
}
