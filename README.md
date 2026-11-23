# TinyBot

TinyBot is a self-hosted Rust agent for Telegram. Each invited user has a Chief
that can delegate to helpers, resume tasks, connect MCP tools, browse public
websites, and run offline Python on task files. Telegram messages steer the
current task. The Chief can generate cards and suggested replies for the
chat. TinyBot sends native HTML widgets where Telegram allows them. General
bots currently receive formatted chat cards and inline buttons because of
[Telegram’s widget restriction](https://github.com/tdlib/telegram-bot-api/issues/947).
`/settings` opens the Mini App for account setup, approval details,
private form inputs, and schedule management.

## Deploy

Use a Linux Docker host with Compose, a public DNS name pointing to the host,
and inbound ports 80 and 443 for Caddy. Create a Telegram bot in BotFather and
use its token and your numeric Telegram user ID. Run one TinyBot replica per
database.

```sh
cp .env.example .env
mkdir -p secrets
umask 077
openssl rand -hex 32 > secrets/encryption_key
# Save the Telegram token in secrets/telegram_bot_token.
# Set TINYBOT_DOMAIN and TELEGRAM_OPERATOR_ID in .env.
docker compose up -d --build
docker compose ps
```

The TinyBot container runs as UID 1000. Make both mounted secret files
readable by that UID, without granting access to other untrusted host users.
Open the bot in a private chat with `/start`. Use `/panel` to see the available
chat controls or `/settings` to open the Mini App. Use `/invite` to invite
another user, or create a read-only invitation from an agent’s Sharing settings.

### Dokploy

Create a **Docker Compose** service using `compose.dokploy.yaml` from the
repository root. Set these variables in the service’s Environment settings.

| Variable | Value |
| --- | --- |
| `TINYBOT_DOMAIN` | The public hostname, such as `bot.example.com` |
| `TELEGRAM_OPERATOR_ID` | Your numeric Telegram user ID |
| `TELEGRAM_BOT_TOKEN` | The token from BotFather |
| `TINYBOT_ENCRYPTION_KEY` | A persistent 32-byte key encoded as 64 hexadecimal characters |

Generate the encryption key with `openssl rand -hex 32` on a trusted machine.
Back it up with your deployment secrets and keep it across redeployments.
Compose passes both credentials as environment variables to the read-only
TinyBot container. Users with Docker access can inspect its environment.

In **Domains**, route the hostname to service `tinybot`, container port
`8080`, with path `/` and HTTPS enabled. The Dokploy Compose entry point
attaches TinyBot to `dokploy-network` so Traefik can terminate TLS. Deploy
after adding the domain. Then check `https://your-domain/health` and send
`/start` to the bot.

## Models and Mini Apps

- **Tasks** use a tool-capable OpenRouter model you select or an eligible ChatGPT
  plan through the official Codex App Server. To set up Codex, create a pairing
  code in the Mini App and run `tinybot codex connect https://your-domain` on a
  computer with a browser. Build the local command with `cargo build --locked
  --release` and use the binary in the build output.
- **Forms** with a single choice can be answered using buttons in chat.
  Ask for a Mini App form to open it directly using **Open form**. After you
  submit it, TinyBot continues the same task in Telegram. TinyBot can use the
  task model to generate fields. When an OpenRouter key is available, it can
  use `inception/mercury-3` for form generation.
  Website values go directly from the Mini App to TinyBot over HTTPS and stay
  in memory until use or expiry. Review the masked preview before submitting.
- **Acknowledgements** use Mercury 3 to generate a short message in your
  language while TinyBot starts a lookup or longer task. A connected OpenRouter
  key enables them for both Codex and OpenRouter tasks.
- **Action review** defaults to a separate call to the active task model. Set
  `TINYBOT_REVIEW_MODEL=typesafe/jev-1.14` to use OpenRouter's typed Decisions API.
  The broker still enforces permissions and approvals. An optional
  `TINYBOT_REVIEW_OPENROUTER_KEY` is the deployment fallback for model review
  and Mercury. Personal OpenRouter credentials take precedence.

## Public websites

The Chief can read public websites from a domain or HTTPS link and search for
public sources. Set `TINYBOT_BRAVE_SEARCH_KEY` to use Brave Search. Without a
key, searches run through the browser worker. Sites may require verification
before TinyBot can read them.

## MCP connectors

Send a message such as **Connect the Atlassian MCP**. TinyBot searches the MCP
Registry and the web, then asks you to approve the selected endpoint and
complete authorization. It can still use registry results if web search is
unavailable.

Remote servers can use Streamable HTTP or legacy HTTP+SSE. Connector setup
supports public endpoints, OAuth with client metadata discovery, dynamic
registration, or a supplied client, and API tokens with a configurable header and prefix. Authorization
depends on what the server and workspace administrator support. A static
OAuth client should register `https://your-domain/oauth/mcp` as its callback.

To use a local stdio server, place operator-reviewed programs and
`programs.json` in the ignored `mcp-programs/` directory. This manifest
provisions a Python MCP server.

```json
{
  "example": {
    "command": ["/usr/local/bin/python", "/programs/example.py"],
    "credential_env": "EXAMPLE_API_TOKEN"
  }
}
```

Run `docker compose --profile stdio up -d --build` and tell TinyBot to connect
`stdio://example`. The worker starts only commands provisioned in this
configuration. It runs offline by default. Servers that need external access
should be deployed behind an HTTPS MCP endpoint.

To delegate a connector read, configure its exact endpoint and tool names in
the operator-owned JSON read policy. The default policy is empty. Choose
the tool and exact JSON arguments in the agent’s Sharing settings. The
connector’s upstream credentials must be scoped to the data you intend to
share. Tools outside the reviewed set require owner approval.

## Tasks and schedules

- `/new` starts a task, `/tasks` lists tasks, `/switch ID` selects a task, and
  `/cancel` stops the active task.
- Say **Remind me tomorrow at 9 am to send the report** or **Draft the report
  every Monday at 9 am**. Use **Schedules** in the Mini App to set an IANA
  time zone and cancel upcoming runs. Cron expressions use five fields in the
  time zone selected when the schedule was created.
- When a schedule is due, your Chief receives the saved prompt in a new task
  and sends the result to Telegram. Use `/tasks` and `/switch ID` to reopen
  the scheduled run.
- If TinyBot restarts during an approved external operation, inspect the
  destination and send `/reconcile` before the task continues. During a
  service outage, a recurring job advances from the current time when TinyBot
  returns. It does not replay every missed occurrence.

## Back up and restore

Run `docker compose exec tinybot tinybot backup /data/snapshot.sqlite` and
copy the resulting snapshot out with `docker compose cp
tinybot:/data/snapshot.sqlite ./snapshot.sqlite`. Use a new snapshot path on
each run because SQLite avoids overwriting an existing backup.

For a restore, stop TinyBot, restore a copy of `snapshot.sqlite` as
`/data/tinybot.sqlite` into a clean application volume, and start TinyBot with
the original encryption key. Keep that key backed up separately from SQLite.
Without the original key, stored provider and connector credentials must be
reconnected.

## Local verification

```sh
cargo test --locked
docker compose config --quiet
```

The test fixtures in `tests/` exercise a sandboxed Chromium form, the Python
worker socket, and an operator-configured stdio MCP. The browser fixture can run
in the browser image with `tests/browser.test.mjs` mounted at
`/opt/tinybot/browser.test.mjs` and Chromium’s Compose seccomp settings. Send
`tests/python_worker.py` to `docker compose exec -T sandbox python -` to check
file mutation, job isolation, and resource limits. Live Telegram, OAuth, and
model-provider checks need accounts and a public hostname.
