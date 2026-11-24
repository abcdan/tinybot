# TinyBot

A self-hosted personal agent you talk to in Telegram. Give your Chief a name,
then ask it to look something up, prepare a reply, or run a task while you’re
away. Reply to an earlier message to pick up where you left off.

> Find three public sources on this, then ask me which one to follow up on.

TinyBot can look up sources, summarize what it found, and give you buttons for
the next step. Open **Settings & forms** in Telegram when you want to manage
connections, review an action, or see what happened during a run.

## Try it in chat

| Ask for | What TinyBot can do |
| --- | --- |
| “Compare these plans and show the sources.” | Browse public pages, format the comparison, and add buttons that open the sources. |
| “Ask Research to check those sources.” | Hand the task to a configured helper and bring the result back to the same chat. |
| “Remember that I prefer short summaries.” | Save the preference and recall it in future conversations. |
| “Draft the weekly update every Monday at 9.” | Give the Chief that prompt on a schedule and send the result to Telegram. |
| “Draft a reply to the email I just opened.” | Show the exact email for one-time approval before SMTP sends it. |

TinyBot also accepts voice messages and uploads, opens Mini App forms, and
connects to services through MCP. The audit log shows which steps ran, what
they returned at a glance, and the token usage and costs the model providers
reported.

## Start your bot

You’ll need a Docker host, a public HTTPS hostname, a Telegram bot token from
[BotFather](https://t.me/BotFather), and an OpenRouter key for first-run setup.
The [setup guide](docs/setup.md) walks through Docker Compose and Dokploy,
including Telegram sign-in, the encryption key, and backups.

Once the secrets and domain are configured, run the standard stack with
`docker compose up -d --build`. On Dokploy, use `compose.dokploy.yaml`.
Both entry points include the self-hosted AnythingMCP gateway. The gateway
has its own hostname and first-admin setup.

Send `/start` to your bot in a private chat, then open `/settings`. Enter
your name and the Chief’s name, connect your key, and pick a tool-capable
model. [See the setup steps](docs/setup.md).

## Explore TinyBot

- [Features and examples](docs/features.md) shows what to ask for and how to use the Telegram controls.
- [Setup and operations](docs/setup.md) covers models, email, the browser proxy, AnythingMCP, Cloudflare, and backups.
- [How data flows](docs/architecture.md) follows a message through the Chief, external tools, approvals, and storage.
