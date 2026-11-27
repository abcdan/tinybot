use super::{Intent, Outcome, Risk, Tool, ToolContext, string};
use crate::store::{
    self,
    outputs::{MediaDelivery, MediaFormat},
};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

pub(super) const CREATE_DOCUMENT: &str = "create_document";
const MAX_DOCUMENT_INPUT: usize = 96 * 1024;

pub(super) struct CreateDocument;
pub(super) struct SendTaskFile;

pub(super) fn requested_native_format(request: &str) -> Option<&'static str> {
    let lower = request.to_lowercase();
    let words: Vec<_> = lower
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let has = |options: &[&str]| words.iter().any(|word| options.contains(word));
    let bare_format: Vec<_> = words
        .iter()
        .copied()
        .filter(|word| {
            !matches!(
                *word,
                "please" | "graag" | "a" | "an" | "een" | "as" | "als" | "in"
            )
        })
        .collect();
    let polite_request = has(&["please", "graag"])
        && matches!(
            bare_format.as_slice(),
            ["pdf"
                | "pdfje"
                | "docx"
                | "pptx"
                | "powerpoint"
                | "xlsx"
                | "excel"
                | "presentation"
                | "presentatie"]
                | ["word", "document" | "bestand"]
                | ["excel", "workbook" | "spreadsheet" | "bestand"]
        );
    let creation_requested = has(&[
        "make",
        "create",
        "generate",
        "export",
        "convert",
        "send",
        "prepare",
        "maak",
        "maken",
        "genereer",
        "genereren",
        "stuur",
        "sturen",
        "exporteer",
        "omzetten",
    ]);
    if !(creation_requested || polite_request)
        || lower.contains("don't")
        || has(&[
            "not",
            "geen",
            "niet",
            "csv",
            "markdown",
            "md",
            "txt",
            "json",
            "outline",
            "opzet",
            "read",
            "summarize",
            "explain",
            "extract",
            "speech",
            "script",
            "lees",
            "samenvatten",
            "uitleg",
        ])
    {
        return None;
    }
    let mut formats = [
        (has(&["pdf", "pdfje"]), "pdf"),
        (
            has(&["docx", "wordbestand"])
                || words
                    .windows(2)
                    .any(|pair| matches!(pair, ["word", "document" | "bestand"])),
            "docx",
        ),
        (
            has(&[
                "pptx",
                "powerpoint",
                "presentation",
                "presentatie",
                "slides",
            ]),
            "pptx",
        ),
        (
            has(&["xlsx", "excel", "workbook", "spreadsheet", "rekenblad"]),
            "xlsx",
        ),
    ]
    .into_iter()
    .filter_map(|(mentioned, format)| mentioned.then_some(format));
    let format = formats.next()?;
    formats.next().is_none().then_some(format)
}

pub(super) fn document_question(text: &str) -> bool {
    text.trim().strip_suffix('?').is_some_and(|question| {
        !question.trim().is_empty()
            && !question.contains(['\n', '?', '!'])
            && !question.contains("sandbox:")
            && !question.split_whitespace().any(|word| word.ends_with('.'))
    })
}

pub(super) fn queued_document(body: &str, extension: &str) -> bool {
    serde_json::from_str::<Value>(body).is_ok_and(|event| {
        event["media_delivery"] == CREATE_DOCUMENT
            && event["queued"] == true
            && event["filename"]
                .as_str()
                .and_then(|name| name.rsplit_once('.'))
                .is_some_and(|(_, actual)| actual == extension)
    })
}

pub(super) async fn missing_document(ctx: &ToolContext<'_>, format: &str) -> Result<()> {
    store::settle(
        &ctx.app.db,
        ctx.task,
        "blocked",
        Some(&format!(
            "I couldn’t create the requested {} file in this turn.",
            format.to_ascii_uppercase()
        )),
    )
    .await
}

pub(super) fn document_correction(format: &str) -> String {
    format!(
        "The requested .{format} file has not been produced. Use create_document with format={format}, using information already available in this task. If a needed fact is missing, ask one specific question."
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DocumentRequest {
    format: String,
    title: String,
    filename: Option<String>,
    blocks: Vec<Value>,
    #[serde(default)]
    continue_task: bool,
}

impl DocumentRequest {
    fn parse(args: &Value) -> Result<(Self, MediaFormat, String)> {
        ensure!(
            args.to_string().len() <= MAX_DOCUMENT_INPUT,
            "The document exceeds its content limit."
        );
        let request: Self = serde_json::from_value(args.clone())
            .context("Supply document blocks and a supported format.")?;
        let format = match request.format.as_str() {
            "pdf" => MediaFormat::Pdf,
            "docx" => MediaFormat::Docx,
            "pptx" => MediaFormat::Pptx,
            "xlsx" => MediaFormat::Xlsx,
            _ => anyhow::bail!("Choose PDF, Word, PowerPoint, or Excel."),
        };
        ensure!(
            !request.title.trim().is_empty()
                && request.title.len() <= 200
                && (1..=40).contains(&request.blocks.len()),
            "Supply a title and one to forty content blocks."
        );
        let filename = if let Some(filename) = &request.filename {
            let base = filename
                .strip_suffix(&format!(".{}", format.extension()))
                .unwrap_or(filename);
            ensure!(
                !base.is_empty()
                    && base.len() <= 80
                    && base
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte)),
                "Choose a simple filename with the requested file extension."
            );
            format!("{base}.{}", format.extension())
        } else {
            let stem = request
                .title
                .split(|ch: char| !ch.is_ascii_alphanumeric())
                .filter(|word| !word.is_empty())
                .collect::<Vec<_>>()
                .join("-");
            let stem = stem
                .get(..stem.len().min(72))
                .unwrap_or_default()
                .trim_end_matches('-');
            format!(
                "{}.{}",
                if stem.is_empty() { "document" } else { stem },
                format.extension()
            )
        };
        Ok((request, format, filename))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_native_file_request_names_one_output_format() {
        assert_eq!(
            requested_native_format("Kan je daar een PDFje van maken met informatie?"),
            Some("pdf")
        );
        assert_eq!(
            requested_native_format("Create a Word document."),
            Some("docx")
        );
        assert_eq!(
            requested_native_format("Maak daar een Word-document van met dezelfde cijfers."),
            Some("docx")
        );
        assert_eq!(
            requested_native_format("Maak hier een presentatie van."),
            Some("pptx")
        );
        assert_eq!(
            requested_native_format("An Excel workbook please"),
            Some("xlsx")
        );
        for message in [
            "How do you print a PDF?",
            "Don't make a PDF.",
            "Please summarize the attached PDF.",
            "Please read this Excel spreadsheet and tell me the totals.",
            "Can you make a speech for my presentation?",
            "Convert PDF to Markdown.",
            "Make an outline for a presentation.",
            "Maak een PDF en een Word document.",
        ] {
            assert_eq!(requested_native_format(message), None, "{message}");
        }
        assert!(document_question("Welke tabel wil je in het document?"));
        assert!(!document_question("I made the PDF. Anything else?"));
    }
}

#[async_trait]
impl Tool for CreateDocument {
    fn name(&self) -> &'static str {
        CREATE_DOCUMENT
    }

    fn describe(&self) -> &'static str {
        "Create and deliver a real PDF, editable Word document, PowerPoint slides, or Excel workbook. Format: pdf, docx, pptx, xlsx. Provide a title and content blocks. Use continue_task only if other requested work remains."
    }

    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{
            "format":{"type":"string","enum":["pdf","docx","pptx","xlsx"]},
            "title":{"type":"string","maxLength":200},
            "filename":{"type":"string","maxLength":85},
            "continue_task":{"type":"boolean"},
            "blocks":{"type":"array","minItems":1,"maxItems":40,"items":{
                "type":"object","properties":{
                    "type":{"type":"string","enum":["heading","paragraph","list","table"]},
                    "text":{"type":"string","maxLength":4000,"description":"Heading up to 200 bytes or paragraph up to 4,000 bytes."},
                    "level":{"type":"integer","minimum":1,"maximum":3},
                    "items":{"type":"array","minItems":1,"maxItems":40,"items":{"type":"string","maxLength":1000}},
                    "rows":{"type":"array","minItems":1,"maxItems":100,"items":{"type":"array","minItems":1,"maxItems":8,
                       "items":{"type":["string","number","boolean"]}}}
                },"required":["type"],"additionalProperties":false
            }}
        },"required":["format","title","blocks"],"additionalProperties":false})
    }

    async fn inspect(&self, ctx: &ToolContext<'_>, args: &Value) -> Result<Intent> {
        ensure!(
            super::media_tools::allowed(ctx) && super::settings::available(ctx).await?,
            "Use the personal Chief in private Telegram chat to create a document."
        );
        DocumentRequest::parse(args)?;
        Ok(Intent {
            risk: Risk::Local,
            destination: "Your private Telegram chat".into(),
            detail: "Create a document".into(),
        })
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> Result<Outcome> {
        ensure!(
            ctx.still_allows(self.name()).await? && super::settings::available(ctx).await?,
            "The task or its permissions changed."
        );
        let (request, format, filename) = DocumentRequest::parse(&args)?;
        let bytes = ctx
            .app
            .workers
            .render_document(
                format.mime(),
                &json!({
                    "format":request.format, "title":request.title, "blocks":request.blocks,
                }),
            )
            .await?;
        ensure!(
            format.valid(&bytes),
            "The worker did not produce a valid native document."
        );
        Ok(Outcome::Media(MediaDelivery {
            tool: self.name(),
            format,
            bytes,
            finish_turn: super::media_tools::can_finish_turn(ctx, !request.continue_task).await?,
            cookie_dialog: None,
            filename: Some(filename),
        }))
    }
}

#[async_trait]
impl Tool for SendTaskFile {
    fn name(&self) -> &'static str {
        "send_task_file"
    }

    fn describe(&self) -> &'static str {
        "Deliver an existing CSV, Markdown, JSON, or plain text task file in the requested text format. Use create_document for PDF, Word, PowerPoint, and Excel."
    }

    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false})
    }

    async fn inspect(&self, ctx: &ToolContext<'_>, args: &Value) -> Result<Intent> {
        ensure!(
            ctx.owner && ctx.parent_task.is_none() && ctx.task.agent.is_none(),
            "only the personal Chief can deliver a task file"
        );
        let name = string(args, "name", 96)?;
        ensure!(
            name.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
                && !name.starts_with('.')
                && name.rsplit_once('.').is_some_and(|(_, extension)| {
                    matches!(
                        extension.to_ascii_lowercase().as_str(),
                        "csv" | "md" | "markdown" | "json" | "txt"
                    )
                }),
            "use a supported task filename"
        );
        Ok(Intent {
            risk: Risk::Local,
            destination: "Your private Telegram chat".into(),
            detail: "Deliver task file".into(),
        })
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> Result<Outcome> {
        let name = string(&args, "name", 96)?;
        let queued =
            store::outputs::queue_document(&ctx.app.db, &ctx.app.vault, ctx.task, name).await?;
        Ok(Outcome::Continue(json!({"queued":queued,"name":name})))
    }
}
