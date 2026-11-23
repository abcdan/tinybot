use super::{ChannelAdapter, media};
use crate::{App, security::clip, store, store::attachments::MAX_ATTACHMENT_BYTES};
use anyhow::Result;
use serde::Deserialize;
use std::time::Duration;

#[cfg(test)]
mod tests;

#[derive(Deserialize)]
pub(super) struct Photo {
    file_id: String,
    file_size: Option<u64>,
    width: u32,
    height: u32,
}

#[derive(Deserialize)]
pub(super) struct Document {
    file_id: String,
    file_size: Option<u64>,
    file_name: Option<String>,
}

pub(super) enum Source {
    Photo(Photo),
    Document(Document),
}

impl Source {
    pub(super) fn select(photos: Vec<Photo>, document: Option<Document>) -> Option<Self> {
        if let Some(document) = document {
            return Some(Self::Document(document));
        }
        photos
            .into_iter()
            .max_by_key(|photo| {
                (
                    !oversized(photo.file_size),
                    u64::from(photo.width) * u64::from(photo.height),
                )
            })
            .map(Self::Photo)
    }

    fn file(&self) -> (&str, Option<u64>) {
        match self {
            Self::Photo(photo) => (&photo.file_id, photo.file_size),
            Self::Document(document) => (&document.file_id, document.file_size),
        }
    }

    fn classify(&self, bytes: &[u8]) -> Option<(&'static str, String)> {
        let image = if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
            Some(("image/jpeg", "photo.jpg"))
        } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(("image/png", "photo.png"))
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            Some(("image/webp", "photo.webp"))
        } else {
            None
        };
        match self {
            Self::Photo(_) => image.map(|(mime, name)| (mime, name.into())),
            Self::Document(document) => {
                let (mime, fallback) = if let Some(image) = image {
                    image
                } else if bytes.starts_with(b"%PDF-") {
                    ("application/pdf", "document.pdf")
                } else if is_text_document(bytes) {
                    ("text/plain", "document.txt")
                } else {
                    return None;
                };
                Some((mime, filename(document.file_name.as_deref(), fallback)))
            }
        }
    }
}

pub struct Received {
    pub(super) external_id: i64,
    pub(super) user: String,
    pub(super) message_id: i64,
    pub(super) input: String,
    pub(super) source: Source,
    pub(super) reply: Option<store::ReplyContext>,
}

pub(super) async fn handle(
    app: &App,
    operator: &str,
    adapter: &dyn ChannelAdapter,
    received: Received,
) -> Result<()> {
    let channel = adapter.name();
    if !media::eligible(
        &app.db,
        channel,
        &received.user,
        received.external_id,
        received.message_id,
    )
    .await?
    {
        return Ok(());
    }
    let (file_id, file_size) = received.source.file();
    let attachment = if oversized(file_size) {
        Err("This file is too large. Send a file up to 8 MB.")
    } else {
        let _ = adapter.typing(&received.user).await;
        match tokio::time::timeout(
            Duration::from_secs(25),
            adapter.download_file(file_id, MAX_ATTACHMENT_BYTES),
        )
        .await
        {
            Ok(Ok(bytes)) if oversized(Some(bytes.len() as u64)) => {
                Err("This file is too large. Send a file up to 8 MB.")
            }
            Ok(Ok(bytes)) => match received.source.classify(&bytes) {
                Some((mime, name)) => Ok(store::attachments::seal(
                    &app.vault,
                    &received.user,
                    received.message_id,
                    name,
                    mime,
                    &bytes,
                )?),
                None => Err(
                    "I can read JPEG, PNG, WebP, PDF and plain-text files, including CSV and Markdown.",
                ),
            },
            _ => Err(
                "I couldn't download that file. Please resend it and check that it's up to 8 MB.",
            ),
        }
    };
    match attachment {
        Ok(attachment) => {
            let input = received.input.trim();
            store::ingest_telegram_attachment(
                &app.db,
                operator,
                received.external_id,
                &received.user,
                if input.is_empty() {
                    "Review the attached file."
                } else {
                    input
                },
                received.message_id,
                received.reply,
                attachment,
            )
            .await
        }
        Err(message) => {
            media::notify(
                &app.db,
                channel,
                &received.user,
                received.external_id,
                message,
                None,
                Some(received.message_id),
            )
            .await
        }
    }
}

fn oversized(size: Option<u64>) -> bool {
    size.is_some_and(|size| size > MAX_ATTACHMENT_BYTES as u64)
}

fn filename(value: Option<&str>, fallback: &str) -> String {
    let name: String = value
        .and_then(|value| value.rsplit(['/', '\\']).next())
        .unwrap_or_default()
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    let name = clip(name.trim(), 120);
    match name {
        "" | "." | ".." => fallback.into(),
        _ => name.into(),
    }
}

fn is_text_document(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok_and(|text| {
        !text.trim().is_empty()
            && !text
                .chars()
                .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    })
}
