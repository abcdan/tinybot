use super::*;
use axum::{Json, Router, http::header, routing::post};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::sync::atomic::{AtomicUsize, Ordering};
use store::outputs::{self, MediaFormat};
use tokio::net::UnixListener;

const PDF_REQUEST: &str = "Kan je daar een PDFje van maken met informatie?";
const WORD_REQUEST: &str = "Maak daar een Word-document van met dezelfde cijfers.";
const PRIOR_REPLY: &str = "Tikka Masala kost €4,46 per maaltijd.";
const PDF: &[u8] = b"%PDF-1.4\nfixture PDF\n%%EOF\n";
const DOCX_PACKAGE: &str = concat!(
    "UEsDBBQAAAAAABWAe10AAAAAAAAAAAAAAAATAAAAW0NvbnRlbnRfVHlwZXNdLnhtbFBLAwQUAAAA",
    "AAAVgHtdAAAAAAAAAAAAAAAAEQAAAHdvcmQvZG9jdW1lbnQueG1sUEsBAhQDFAAAAAAAFYB7XQAA",
    "AAAAAAAAAAAAABMAAAAAAAAAAAAAAIABAAAAAFtDb250ZW50X1R5cGVzXS54bWxQSwECFAMUAAAA",
    "AAAVgHtdAAAAAAAAAAAAAAAAEQAAAAAAAAAAAAAAgAExAAAAd29yZC9kb2N1bWVudC54bWxQSwUG",
    "AAAAAAIAAgCAAAAAYAAAAAAA",
);

fn blocks() -> Value {
    json!([{"type":"table","rows":[["Maaltijd","Prijs"],["Tikka Masala","€4,46"]]}])
}

struct DocumentChief {
    request: &'static str,
    format: MediaFormat,
    first: Option<Value>,
    calls: AtomicUsize,
}

#[async_trait]
impl ModelProvider for DocumentChief {
    async fn complete(&self, system: &str, input: &str) -> Result<String> {
        self.complete_turn(system, input, &[], None).await
    }

    async fn complete_turn(
        &self,
        _: &str,
        input: &str,
        tools: &[ToolDefinition],
        _: Option<&mut TextDelta<'_>>,
    ) -> Result<String> {
        let context: Value = serde_json::from_str(input)?;
        assert_eq!(context["latest_request"], self.request);
        assert!(
            context["history"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| { event["role"] == "assistant" && event["text"] == PRIOR_REPLY })
        );
        assert!(tools.iter().any(|tool| tool.name == "create_document"));

        let call = self.calls.fetch_add(1, Ordering::Relaxed);
        if call == 0
            && let Some(first) = &self.first
        {
            return Ok(first.to_string());
        }
        assert_eq!(call, usize::from(self.first.is_some()));
        Ok(json!({
            "kind":"tool", "name":"create_document", "args":{
                "format":self.format.extension(), "title":"Maaltijdvergelijking",
                "filename":"maaltijdvergelijking", "blocks":blocks(),
            },
        })
        .to_string())
    }
}

#[tokio::test]
async fn native_file_followups_reuse_context_and_do_not_send_markdown_or_false_success() {
    for (request, format, first, bytes) in [
        (
            PDF_REQUEST,
            MediaFormat::Pdf,
            Some(json!({"kind":"tool","name":"send_task_file","args":{"name":"comparison.md"}})),
            PDF.to_vec(),
        ),
        (
            PDF_REQUEST,
            MediaFormat::Pdf,
            Some(json!({"kind":"reply","text":"Ik heb het PDF’je gemaakt. Download het hier."})),
            PDF.to_vec(),
        ),
        (
            WORD_REQUEST,
            MediaFormat::Docx,
            None,
            STANDARD.decode(DOCX_PACKAGE).unwrap(),
        ),
    ] {
        let db = fixture().await;
        let model = Arc::new(DocumentChief {
            request,
            format,
            first,
            calls: AtomicUsize::new(0),
        });
        let mut app = app_with_model(&db, model.clone());
        let path = std::env::temp_dir().join(format!("tinybot-doc-{}.sock", &security::id()[..12]));
        let unix = UnixListener::bind(&path).unwrap();
        app.workers.python = path.to_str().unwrap().into();
        let rendered = bytes.clone();
        let worker = Router::new().route("/render-document", post(move |Json(body): Json<Value>| {
            let rendered = rendered.clone();
            async move {
                assert_eq!(body, json!({
                    "format":format.extension(), "title":"Maaltijdvergelijking", "blocks":blocks(),
                }));
                ([(header::CONTENT_TYPE, format.mime())], rendered)
            }
        }));
        let server = tokio::spawn(async move { axum::serve(unix, worker).await.unwrap() });

        store::ingest_telegram(&db, "1", 1, "1", "Vergelijk de maaltijdprijzen.", 26)
            .await
            .unwrap();
        let initial = store::claim(&db).await.unwrap().unwrap();
        store::append(&db, &initial.id, "assistant", PRIOR_REPLY)
            .await
            .unwrap();
        sqlx::query("INSERT INTO files(task,name,body) VALUES(?,'comparison.md',?)")
            .bind(&initial.id)
            .bind(PRIOR_REPLY)
            .execute(&db)
            .await
            .unwrap();
        store::settle(&db, &initial, "idle", None).await.unwrap();

        store::ingest_telegram(&db, "1", 2, "1", request, 27)
            .await
            .unwrap();
        let followup = store::claim(&db).await.unwrap().unwrap();
        assert_eq!(initial.id, followup.id);
        let work = runtime::work(&app, &followup).await;
        server.abort();
        std::fs::remove_file(path).unwrap();
        work.unwrap();

        let outbound = store::claim_outbox(&db).await.unwrap().unwrap();
        assert_eq!(outbound.reply_to_message_id, Some(27));
        assert!(outbound.body.is_empty());
        let output = outputs::load(&db, &app.vault, &outbound)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(output.kind, format.extension());
        assert_eq!(
            output.name,
            format!("maaltijdvergelijking.{}", format.extension())
        );
        assert_eq!(output.bytes, bytes);
        assert!(output.ephemeral);
        assert!(store::claim_outbox(&db).await.unwrap().is_none());
        assert_eq!(task_for(&db, &followup.id).await.state, "idle");
        assert_eq!(
            model.calls.load(Ordering::Relaxed),
            1 + usize::from(model.first.is_some())
        );
    }
}
