use super::ChannelAdapter;
use crate::store::{self, groups::GroupRoute};
use anyhow::Result;

pub struct Message {
    pub(super) external: i64,
    pub(super) user: String,
    pub(super) route: GroupRoute,
    pub(super) message: i64,
    pub(super) text: String,
    pub(super) reply: Option<(i64, i64)>,
}

pub(super) async fn handle(
    app: &crate::App,
    adapter: &dyn ChannelAdapter,
    input: Message,
) -> Result<()> {
    let (command, text) = input
        .text
        .split_once(char::is_whitespace)
        .map_or((input.text.as_str(), ""), |(command, text)| {
            (command, text.trim())
        });
    let command = command
        .split_once('@')
        .filter(|(name, _)| matches!(*name, "/ask" | "/tinybot_enable"));
    if command.is_none() && (input.reply.is_none() || input.text.starts_with('/')) {
        return Ok(());
    }
    if !store::invitations::available(&app.db, &input.user).await? {
        return Ok(());
    }
    let (bot, username) = adapter.bot_identity().await?;
    if let Some((command, target)) = command {
        if !target.eq_ignore_ascii_case(&username) {
            return Ok(());
        }
        if command == "/tinybot_enable" {
            if adapter.group_admin(input.route.chat, &input.user).await?
                && store::groups::request(&app.db, &app.operator, &input.user, input.route.chat)
                    .await?
            {
                adapter
                    .send_group(
                        input.route,
                        "Group access requested. The TinyBot operator can approve it.",
                        Some(input.message),
                    )
                    .await?;
            }
        } else {
            store::groups::input(
                &app.db,
                &input.user,
                input.external,
                input.route,
                input.message,
                None,
                text,
            )
            .await?;
        }
    } else if let Some((_, reply)) = input.reply.filter(|(author, _)| *author == bot) {
        store::groups::input(
            &app.db,
            &input.user,
            input.external,
            input.route,
            input.message,
            Some(reply),
            &input.text,
        )
        .await?;
    }
    Ok(())
}
