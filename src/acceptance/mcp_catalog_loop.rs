use super::{app_with_model, fixture, task_for};
use crate::{
    model::{ModelProvider, TextDelta, ToolDefinition},
    runtime::{self, Intent, Outcome, Tool, ToolContext},
    store,
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const CONNECTOR: &str = "summary-fixture";
const ENDPOINT: &str = "stdio:///summary-fixture";
const NOTE: &str = "Yesterday we shipped the fixture report.";
const NO_DATA: &str = "I found the connector, but couldn't read a daily summary.";

struct Chief {
    varied: bool,
    calls: AtomicUsize,
}

#[async_trait]
impl ModelProvider for Chief {
    async fn complete(&self, system: &str, input: &str) -> Result<String> {
        if system.contains("Give a final answer") {
            assert!(self.varied);
            assert!(input.contains("Connector discovery has stopped"));
            return Ok(json!({"text":NO_DATA}).to_string());
        }
        let input: Value = serde_json::from_str(input)?;
        assert!(system.starts_with("Review this proposed TinyBot tool action"));
        assert_eq!(input["operation"], "MCP tool get_note");
        Ok(json!({"verdict":"allow","reason":"Read one requested record."}).to_string())
    }

    async fn complete_turn(
        &self,
        _: &str,
        input: &str,
        tools: &[ToolDefinition],
        _: Option<&mut TextDelta<'_>>,
    ) -> Result<String> {
        let pass = self.calls.fetch_add(1, Ordering::Relaxed);
        assert!(pass < 7, "catalog inspection kept looping");
        assert!(tools.iter().any(|tool| tool.name == "list_mcp_tools"));
        let input: Value = serde_json::from_str(input)?;
        let last = input["history"].as_array().unwrap().last().unwrap()["text"]
            .as_str()
            .unwrap();
        let action = if last.contains(NOTE) {
            json!({"kind":"reply","text":NOTE})
        } else if !self.varied && last.contains("Use the previously returned schemas") {
            json!({"kind":"tool","name":"call_mcp","args":{
                "connector":CONNECTOR,"tool":"get_note","arguments":{"method":"GET","title":"Daily summary"}
            }})
        } else {
            let search = if self.varied {
                format!("summary keyword {pass}")
            } else if pass == 0 {
                "  Daily   Notes ".to_owned()
            } else {
                "daily notes".to_owned()
            };
            json!({"kind":"tool","name":"list_mcp_tools","args":{
                "connector":if pass == 0 {"Summary Fixture"} else {CONNECTOR},
                "search":search,
            }})
        };
        Ok(action.to_string())
    }
}

struct ConnectorFixture {
    builtin: Box<dyn Tool>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for ConnectorFixture {
    fn name(&self) -> &'static str {
        self.builtin.name()
    }

    fn describe(&self) -> &'static str {
        self.builtin.describe()
    }

    fn input_schema(&self) -> Value {
        self.builtin.input_schema()
    }

    async fn inspect(&self, ctx: &ToolContext<'_>, args: &Value) -> Result<Intent> {
        self.builtin.inspect(ctx, args).await
    }

    async fn execute(&self, _: &ToolContext<'_>, args: Value) -> Result<Outcome> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        assert_eq!(args["connector"], CONNECTOR);
        let result = if self.name() == "list_mcp_tools" {
            assert_eq!(
                args["search"].as_str().unwrap(),
                args["search"].as_str().unwrap().to_lowercase()
            );
            json!({"tools":[{"name":"get_note"}],"total":1})
        } else {
            assert_eq!(args["tool"], "get_note");
            json!({"connector":"Summary Fixture","result":NOTE})
        };
        Ok(Outcome::Continue(result))
    }
}

#[tokio::test]
async fn catalog_repeats_yield_to_the_read_or_finish_with_a_clear_limit() {
    for (varied, existing) in [(false, false), (true, false), (true, true)] {
        let db = fixture().await;
        sqlx::query(
            "INSERT INTO connectors(id,owner,name,endpoint,auth,transport)
            VALUES(?,'1','Summary Fixture',?,'public','stdio')",
        )
        .bind(CONNECTOR)
        .bind(ENDPOINT)
        .execute(&db)
        .await
        .unwrap();
        let chief = Arc::new(Chief {
            varied,
            calls: AtomicUsize::new(0),
        });
        let mut app = app_with_model(&db, chief.clone());
        app.mcp = app.mcp.with_read_policy(ENDPOINT, &["get_note"]);
        let catalog_calls = Arc::new(AtomicUsize::new(0));
        let read_calls = Arc::new(AtomicUsize::new(0));
        for (name, calls) in [
            ("list_mcp_tools", &catalog_calls),
            ("call_mcp", &read_calls),
        ] {
            let index = app
                .tools
                .iter()
                .position(|tool| tool.name() == name)
                .unwrap();
            let builtin = app.tools.remove(index);
            app.tools.push(Box::new(ConnectorFixture {
                builtin,
                calls: calls.clone(),
            }));
        }

        store::ingest_telegram(
            &db,
            "1",
            1,
            "1",
            "Kijk naar mijn dagelijkse samenvattingen dan",
            21,
        )
        .await
        .unwrap();
        let task = store::claim(&db).await.unwrap().unwrap();
        if existing {
            for index in 0..4 {
                assert!(
                    store::tool_budget::record(
                        &db,
                        &task,
                        "chief",
                        "list_mcp_tools",
                        &json!({"connector":"Summary Fixture","search":format!("previous {index}")})
                    )
                    .await
                    .unwrap()
                );
            }
        }
        runtime::work(&app, &task).await.unwrap();
        assert!(store::list_requests(&db, "1").await.unwrap().is_empty());
        assert_eq!(task_for(&db, &task.id).await.state, "idle");
        assert_eq!(
            catalog_calls.load(Ordering::Relaxed),
            if existing {
                0
            } else if varied {
                4
            } else {
                1
            }
        );
        assert_eq!(read_calls.load(Ordering::Relaxed), usize::from(!varied));
        assert_eq!(
            chief.calls.load(Ordering::Relaxed),
            if existing {
                2
            } else if varied {
                6
            } else {
                4
            }
        );
        assert_eq!(
            store::claim_outbox(&db).await.unwrap().unwrap().body,
            if varied { NO_DATA } else { NOTE }
        );
    }
}
