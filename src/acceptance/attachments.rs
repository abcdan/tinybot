use super::{app_with_model, fixture};
use crate::{
    model::{AttachmentInput, AttachmentRead, ModelProvider},
    runtime, store,
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct AttachmentChief(AtomicUsize);

const PHOTO: &[u8] = &[0xff, 0xd8, 0xff, 0xdb, 0xff, 0xd9];
const REPORT: &str = "The photo shows the label Sunset Ridge. Private reading fixture.";

#[async_trait]
impl ModelProvider for AttachmentChief {
    async fn read_attachment(
        &self,
        input: &str,
        attachment: AttachmentInput<'_>,
    ) -> Result<Option<AttachmentRead>> {
        let AttachmentInput::Image { mime, bytes } = attachment else {
            panic!("expected an image");
        };
        assert_eq!(mime, "image/jpeg");
        assert_eq!(bytes, PHOTO);
        let context: Value = serde_json::from_str(input)?;
        assert_eq!(context["filename"], "test.jpg");
        let attempt = self.0.fetch_add(1, Ordering::Relaxed);
        assert_eq!(
            context["latest_user_request"],
            match attempt {
                0 => "Read the attached sign.",
                1 => "Read it again.",
                _ => panic!("unexpected attachment call"),
            }
        );
        Ok(Some(AttachmentRead {
            text: REPORT.into(),
            limitation: None,
        }))
    }

    async fn complete(&self, system: &str, input: &str) -> Result<String> {
        assert!(system.contains("Use the attachment field to read a Telegram upload"));
        let context: Value = serde_json::from_str(input)?;
        assert_eq!(context["attachment"]["status"], "read");
        let response = match context["latest_request"].as_str() {
            Some("Read the attached sign." | "Read it again.") => {
                assert_eq!(context["attachment"]["content"], REPORT);
                assert!(!context["history"].to_string().contains(REPORT));
                "The sign reads Sunset Ridge."
            }
            Some("Read this text file.") => {
                assert_eq!(context["attachment"]["content"], "Text-file fixture");
                "I read the text file."
            }
            _ => panic!("unexpected Chief turn"),
        };
        Ok(json!({"kind":"reply","text":response}).to_string())
    }
}

#[tokio::test]
async fn chief_reads_telegram_attachments_and_rechecks_a_quoted_upload() {
    let db = fixture().await;
    let model = Arc::new(AttachmentChief(AtomicUsize::new(0)));
    let app = app_with_model(&db, model.clone());
    let photo =
        store::attachments::seal(&app.vault, "1", 50, "test.jpg".into(), "image/jpeg", PHOTO)
            .unwrap();
    store::ingest_telegram_attachment(&db, "1", 1, "1", "Read the attached sign.", 50, None, photo)
        .await
        .unwrap();
    let first = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &first).await.unwrap();
    assert_eq!(
        store::claim_outbox(&db).await.unwrap().unwrap().body,
        "The sign reads Sunset Ridge."
    );

    store::ingest_telegram_reply(
        &db,
        "1",
        2,
        "1",
        "Read it again.",
        51,
        Some(store::ReplyContext {
            message_id: 50,
            excerpt: Some("Read the attached sign.".into()),
            from_bot: false,
        }),
    )
    .await
    .unwrap();
    let follow_up = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(follow_up.id, first.id);
    runtime::work(&app, &follow_up).await.unwrap();
    assert_eq!(
        store::claim_outbox(&db).await.unwrap().unwrap().body,
        "The sign reads Sunset Ridge."
    );

    let doc = store::attachments::seal(
        &app.vault,
        "1",
        52,
        "test.txt".into(),
        "text/plain",
        b"Text-file fixture",
    )
    .unwrap();
    store::ingest_telegram_attachment(&db, "1", 3, "1", "Read this text file.", 52, None, doc)
        .await
        .unwrap();
    let third = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &third).await.unwrap();
    assert_eq!(
        store::claim_outbox(&db).await.unwrap().unwrap().body,
        "I read the text file."
    );
    assert_eq!(model.0.load(Ordering::Relaxed), 2);
}
