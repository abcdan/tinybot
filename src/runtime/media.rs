use crate::{
    App, audit,
    model::{AttachmentInput, AttachmentRead, ModelProvider},
    security::clip,
    store::{self, Task, attachments::Attachment},
};
use anyhow::Result;
use serde_json::{Value, json};

const MAX_ATTACHMENT_TEXT: usize = 12_000;

pub(super) async fn context(
    app: &App,
    task: &Task,
    model: &dyn ModelProvider,
    request: Option<&str>,
) -> Result<Value> {
    let Some(attachment) = store::attachments::latest(&app.db, &app.vault, task).await? else {
        return Ok(Value::Null);
    };
    if !store::current(&app.db, task).await? {
        return Ok(Value::Null);
    }

    let mut value =
        json!({"message_event":attachment.event_id,"name":attachment.name,"mime":attachment.mime});
    match read(model, &attachment, request).await {
        Ok(Some(read)) => {
            let text = read.text.trim();
            value["status"] = json!("read");
            value["content"] = json!(clip(text, MAX_ATTACHMENT_TEXT));
            value["truncated"] =
                json!(read.limitation.is_some() || text.len() > MAX_ATTACHMENT_TEXT);
            if let Some(limit) = read.limitation {
                value["limit"] = json!(limit);
            }
        }
        Ok(None) => value["status"] = json!("unsupported"),
        Err(_) => value["status"] = json!("unreadable"),
    }
    Ok(value)
}

async fn read(
    model: &dyn ModelProvider,
    attachment: &Attachment,
    request: Option<&str>,
) -> Result<Option<AttachmentRead>> {
    let bytes = &attachment.bytes;
    let attachment_input = match attachment.mime.as_str() {
        "text/plain" => {
            let text = std::str::from_utf8(bytes)?.trim_start_matches('\u{feff}');
            let text = text.trim();
            anyhow::ensure!(!text.is_empty(), "empty attachment text");
            return Ok(Some(AttachmentRead {
                text: clip(text, MAX_ATTACHMENT_TEXT).to_owned(),
                limitation: (text.len() > MAX_ATTACHMENT_TEXT)
                    .then_some("The full text file exceeds the amount that fits in one turn."),
            }));
        }
        mime @ ("image/jpeg" | "image/png" | "image/webp") => {
            AttachmentInput::Image { mime, bytes }
        }
        "application/pdf" => AttachmentInput::Pdf {
            name: &attachment.name,
            bytes,
        },
        _ => return Ok(None),
    };
    let input = json!({
        "latest_user_request":request.map(|text| clip(text, 8_000)),
        "filename":attachment.name,
    });
    audit::model("attachment_read", async {
        model
            .read_attachment(&input.to_string(), attachment_input)
            .await
    })
    .await
}
