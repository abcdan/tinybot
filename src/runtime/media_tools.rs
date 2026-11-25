use super::{Intent, Outcome, Risk, Tool, ToolContext, string};
use crate::{
    audit,
    model::MAX_IMAGE_PROMPT_BYTES,
    net, store,
    store::outputs::{MediaDelivery, MediaFormat},
};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use url::{Host, Url};

const SCREENSHOT: &str = "screenshot_page";
const RECORD: &str = "record_page";
const GENERATE: &str = "generate_image";
const EDIT: &str = "edit_image";
const PINNED_MODEL: &str = "_image_model";

enum MediaTool {
    Screenshot,
    Record,
    Generate,
    Edit,
}

pub(super) fn tools() -> Vec<Box<dyn Tool>> {
    [
        MediaTool::Screenshot,
        MediaTool::Record,
        MediaTool::Generate,
        MediaTool::Edit,
    ]
    .into_iter()
    .map(|tool| Box::new(tool) as Box<dyn Tool>)
    .collect()
}

pub(super) fn is_media_tool(name: &str) -> bool {
    is_image_tool(name) || matches!(name, SCREENSHOT | RECORD)
}

pub(super) fn is_image_tool(name: &str) -> bool {
    matches!(name, GENERATE | EDIT)
}

pub(super) fn allowed(ctx: &ToolContext<'_>) -> bool {
    ctx.owner
        && ctx.parent_task.is_none()
        && ctx.task.agent.is_none()
        && ctx.task.shared_memory_pair.is_none()
        && ctx.allowed_tools.is_none()
}

pub(super) async fn pin_image_model(
    ctx: &ToolContext<'_>,
    name: &str,
    args: &mut Value,
) -> Result<()> {
    if is_image_tool(name)
        && let Some(args) = args.as_object_mut()
    {
        let selected = ctx.app.models.image_model(&ctx.task.user).await?;
        args.insert(PINNED_MODEL.to_owned(), json!(selected));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    url: String,
    dismiss_cookie_dialog: Option<bool>,
    #[serde(default)]
    continue_task: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Recording {
    url: String,
    dismiss_cookie_dialog: Option<bool>,
    duration_seconds: Option<u8>,
    #[serde(default)]
    scroll: bool,
    #[serde(default)]
    continue_task: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageRequest {
    prompt: String,
    #[serde(rename = "_image_model")]
    model: String,
    #[serde(default)]
    continue_task: bool,
}

struct CaptureRequest {
    url: Url,
    body: Value,
    route: &'static str,
    mime: &'static str,
    format: MediaFormat,
    continue_task: bool,
}

fn public_page(value: &str) -> Result<Url> {
    ensure!(
        !value.is_empty() && value.len() <= 2048,
        "provide a public page URL"
    );
    let url = net::website(value)?;
    match url.host() {
        Some(Host::Ipv4(ip)) => {
            ensure!(net::public(ip.into()), "capture destination is not public")
        }
        Some(Host::Ipv6(ip)) => {
            ensure!(net::public(ip.into()), "capture destination is not public")
        }
        _ => {}
    }
    Ok(url)
}

impl MediaTool {
    fn capture(&self, args: &Value) -> Result<CaptureRequest> {
        match self {
            Self::Screenshot => {
                let page: Page = serde_json::from_value(args.clone())?;
                let url = public_page(&page.url)?;
                let body = json!({"url":url.as_str(),
                    "dismiss_cookie_dialog":page.dismiss_cookie_dialog.unwrap_or(true)});
                Ok(CaptureRequest {
                    url,
                    body,
                    route: "screenshot",
                    mime: "image/png",
                    format: MediaFormat::Png,
                    continue_task: page.continue_task,
                })
            }
            Self::Record => {
                let recording: Recording = serde_json::from_value(args.clone())?;
                let seconds = recording.duration_seconds.unwrap_or(5);
                ensure!(
                    (2..=10).contains(&seconds),
                    "recordings must be between 2 and 10 seconds"
                );
                let url = public_page(&recording.url)?;
                let body = json!({"url":url.as_str(),"duration_seconds":seconds,"scroll":recording.scroll,
                    "dismiss_cookie_dialog":recording.dismiss_cookie_dialog.unwrap_or(true)});
                Ok(CaptureRequest {
                    url,
                    body,
                    route: "record",
                    mime: "video/mp4",
                    format: MediaFormat::Mp4,
                    continue_task: recording.continue_task,
                })
            }
            _ => unreachable!("only browser tools capture pages"),
        }
    }

    async fn image_request(ctx: &ToolContext<'_>, args: &Value) -> Result<ImageRequest> {
        string(args, "prompt", MAX_IMAGE_PROMPT_BYTES)?;
        let request: ImageRequest = serde_json::from_value(args.clone())?;
        let current_model = ctx.app.models.image_model(&ctx.task.user).await?;
        ensure!(
            request.model == current_model,
            "The selected image model changed. Ask for a new image request."
        );
        Ok(request)
    }
}

#[async_trait]
impl Tool for MediaTool {
    fn name(&self) -> &'static str {
        match self {
            Self::Screenshot => SCREENSHOT,
            Self::Record => RECORD,
            Self::Generate => GENERATE,
            Self::Edit => EDIT,
        }
    }

    fn describe(&self) -> &'static str {
        match self {
            Self::Screenshot => {
                "Capture a screenshot of a public page at url and deliver it to the user's private Telegram chat."
            }
            Self::Record => {
                "Record a public page at url and deliver the MP4 to private Telegram. Optional duration_seconds (2–10, default 5) and scroll (boolean)."
            }
            Self::Generate => {
                "Generate a requested image from prompt using the user's selected image model and deliver it to private Telegram."
            }
            Self::Edit => {
                "Edit the user's uploaded or explicitly quoted PNG, JPEG, or WebP using prompt and deliver it to private Telegram. Requires an uploaded image."
            }
        }
    }

    fn input_schema(&self) -> Value {
        let (mut properties, required) = match self {
            Self::Screenshot => (json!({"url":{"type":"string","maxLength":2048}}), "url"),
            Self::Record => (
                json!({"url":{"type":"string","maxLength":2048},
                "duration_seconds":{"type":"integer","minimum":2,"maximum":10},"scroll":{"type":"boolean"}}),
                "url",
            ),
            Self::Generate | Self::Edit => (
                json!({"prompt":{"type":"string","maxLength":MAX_IMAGE_PROMPT_BYTES}}),
                "prompt",
            ),
        };
        properties["continue_task"] = json!({"type":"boolean","description":
            "Set true when there is other requested work after this media. Defaults to false."});
        if matches!(self, Self::Screenshot | Self::Record) {
            properties["dismiss_cookie_dialog"] = json!({"type":"boolean","description":
                "Use a functional-only or reject option on recognized cookie dialogs. Defaults to true. Set false to capture the cookie dialog itself."});
        }
        json!({"type":"object","properties":properties,"required":[required],"additionalProperties":false})
    }

    async fn inspect(&self, ctx: &ToolContext<'_>, args: &Value) -> Result<Intent> {
        ensure!(
            allowed(ctx) && super::settings::available(ctx).await?,
            "use the personal Chief in a private chat to create media"
        );
        let (risk, destination, detail) = match self {
            Self::Screenshot | Self::Record => {
                let request = self.capture(args)?;
                let action = if matches!(self, Self::Screenshot) {
                    "Capture a page screenshot"
                } else {
                    "Record a short page video"
                };
                (
                    Risk::PublicRead,
                    request.url.origin().ascii_serialization(),
                    action,
                )
            }
            Self::Generate | Self::Edit => {
                let request = Self::image_request(ctx, args).await?;
                let action = if matches!(self, Self::Generate) {
                    "Generate a requested image"
                } else {
                    "Edit the user's uploaded image"
                };
                (
                    Risk::Read,
                    format!("OpenRouter ({})", request.model),
                    action,
                )
            }
        };
        Ok(Intent {
            risk,
            destination,
            detail: detail.into(),
        })
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> Result<Outcome> {
        ensure!(
            ctx.still_allows(self.name()).await? && super::settings::available(ctx).await?,
            "task or authorization changed"
        );
        let app = ctx.app;
        let (format, bytes, finish_turn, cookie_dialog) = match self {
            Self::Screenshot | Self::Record => {
                let request = self.capture(&args)?;
                let capture = app
                    .workers
                    .browser_capture(request.route, request.mime, &request.body)
                    .await?;
                let blocked = request.body["dismiss_cookie_dialog"] == true
                    && capture.cookie_dialog == Some("visible");
                (
                    request.format,
                    capture.bytes,
                    !request.continue_task && !blocked,
                    capture.cookie_dialog,
                )
            }
            Self::Generate | Self::Edit => {
                let request = Self::image_request(ctx, &args).await?;
                let attachment = if matches!(self, Self::Edit) {
                    Some(store::attachments::latest(&app.db, &app.vault, ctx.task).await?
                        .filter(|file| matches!(file.mime.as_str(), "image/png" | "image/jpeg" | "image/webp"))
                        .context("Attach a PNG, JPEG, or WebP, or reply directly to your earlier image upload.")?)
                } else {
                    None
                };
                let source = attachment
                    .as_ref()
                    .map(|file| (file.mime.as_str(), file.bytes.as_slice()));
                let generated = audit::model(
                    self.name(),
                    app.models.render_image(
                        &ctx.task.user,
                        &request.model,
                        request.prompt.trim(),
                        source,
                    ),
                )
                .await?;
                let format = match generated.mime {
                    "image/png" => MediaFormat::Png,
                    "image/jpeg" => MediaFormat::Jpeg,
                    "image/webp" => MediaFormat::Webp,
                    _ => anyhow::bail!("unsupported image format"),
                };
                (format, generated.bytes, !request.continue_task, None)
            }
        };
        let unfinished_plan = finish_turn
            && store::checkpoints::plan(&app.db, &ctx.task.id, ctx.task.revision, false)
                .await?
                .iter()
                .any(|step| step.completed_at.is_none());
        Ok(Outcome::Media(MediaDelivery {
            tool: self.name(),
            format,
            bytes,
            finish_turn: finish_turn && !unfinished_plan,
            cookie_dialog,
        }))
    }
}
