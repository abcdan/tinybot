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

Set the BotFather Main Mini App URL to your public HTTPS domain or its `/mini`
path. When people open the app from the bot, Telegram signs them in using their
existing account. TinyBot allows the configured operator and invited users.
On the first launch, enter your name, your Chief’s name, and an OpenRouter
key. Choose a tool-capable model to finish setup and open the settings tabs.
Configured accounts can continue using their existing connection.

- **Tasks** use a tool-capable OpenRouter model or an eligible ChatGPT plan
  through the official Codex App Server. Connect your OpenRouter API key in the
  Mini App to load the models available to your account, then choose a task
  model. The same key enables Mercury for forms and acknowledgements, including
  when Codex is the active task model.
- **Voice notes** in private Telegram chats use your saved OpenRouter key to
  transcribe recordings up to 10 MB. Chief receives the transcript as a chat
  message prefixed with `Voice message:`, including when Codex is your active
  task model. TinyBot sends the audio to OpenRouter for transcription and
  keeps the transcript in task history.
- **Photos and files** can be sent in a private chat with a question as the
  caption. TinyBot accepts JPEG, PNG, WebP, PDF and UTF-8 text such as CSV or
  Markdown up to 8 MB per file. Images require a vision-capable task model.
  OpenRouter can read PDFs through its file input. With Codex, TinyBot extracts
  text from up to the first 12 pages locally. Reply directly to an earlier
  upload to use it again in the same task. Stored attachment bytes are encrypted.
- **Codex** pairing uses a code from the Mini App. Run `tinybot codex connect
  https://your-domain` on a computer with a browser. Build the local command
  with `cargo build --locked --release` and use the binary in the build output.
- **Forms** with a single choice can be answered using buttons in chat.
  Ask for a Mini App form to open it directly using **Open form**. After you
  submit it, TinyBot continues the same task in Telegram. TinyBot can use the
  task model to generate fields. When an OpenRouter key is available, it can
  use `inception/mercury-3` for form generation.
  Website values go directly from the Mini App to TinyBot over HTTPS and stay
  in memory until use or expiry. Review the masked preview before submitting.
- **Acknowledgements** use Mercury 3 to generate a short message in your
  language while TinyBot starts a lookup or longer task.
- **Action review** defaults to a separate call to the active task model. Set
  `TINYBOT_REVIEW_MODEL=typesafe/jev-1.14` to use OpenRouter's typed Decisions API.
  The broker still enforces permissions and approvals. An optional
  `TINYBOT_REVIEW_OPENROUTER_KEY` is the deployment fallback for model review
  and Mercury. Personal OpenRouter credentials take precedence.

## Email

Open **Email** in the Telegram Mini App to add IMAP and SMTP accounts. IMAP uses
port 993. SMTP uses port 465 with TLS or port 587 with required STARTTLS. Use
your provider's public mail hostnames and credentials for the corresponding
protocols. TinyBot encrypts the credentials and retrieved mail.

IMAP monitors the folders you configure. On first connection, it starts at the
current mailbox position and watches for new mail. Each folder starts in
**Notify only** mode. The Telegram notice hides the sender, subject and body.
Tap **Read more** to view the message and select it for your next instruction,
or reply directly to the notice with what you want the Chief to do. This
integration reads the text of the email.
Enable automation on a folder to have the Chief summarize new mail or follow
your handling instruction. Automated turns can reply in Telegram, ask for
input, or prepare an email draft for review.

Ask the Chief in chat to compose or reply to an email. Review the exact sender,
recipients, subject and body, then tap **Approve once** or **Deny**. Every SMTP
send requires its own approval. TinyBot reports when the server accepts the
message for delivery.

## Public websites

In **Model → OpenRouter tools**, toggle Web search, Web fetch, Date and time,
and Search models individually. They are enabled by default when OpenRouter
is the active task provider. Hosted search and fetch are unavailable while
a browser proxy is configured. The local browser and search still work.

The Chief can read public websites from a domain or HTTPS link and search for
public sources. Set `TINYBOT_BRAVE_SEARCH_KEY` to use Brave Search. Without a
key, searches run through the browser worker. Sites may require verification
before TinyBot can read them.

### Browser egress proxy

Open **Settings & forms → Browser** in Telegram as the TinyBot operator to save
an upstream proxy, use a direct connection, or restore the deployment setting.
Changes apply to new browser connections. The Mini App accepts public proxy
endpoints and keeps the URL and credentials out of its status response.

For a deployment default, set `TINYBOT_BROWSER_PROXY` in `.env` or Dokploy's
**Environment** settings. An empty deployment setting uses direct connections.

| Proxy type | URL format | Port |
| --- | --- | --- |
| SOCKS5 | `socks5://[username:password@]host:port` | Required |
| HTTP CONNECT | `http://[username:password@]host:port` | Defaults to 80 |

Use a trusted proxy. Credentials are optional, and HTTP proxy authentication
uses Basic. Percent-encode reserved characters in credentials, such as `@`
as `%40`. The setting routes only browser worker traffic, including
browser-backed searches, and retains DNS and public HTTPS destination checks.
The Brave Search API uses a separate connection when configured. With a
proxy set, browser traffic uses it exclusively and proxy failures surface as
request errors.

## Agent audit log

Open **Settings & forms → Audit** in Telegram to inspect new message runs and
scheduled tasks. Each run shows its model calls, tool calls and outcome
summaries, including work that resumes after an approval or form submission.
The log keeps seven days of activity. Entries summarize sensitive tool
payloads and omit private form values, credentials, and raw model prompts.

**Model usage** shows reported costs in USD and input, output, and total
tokens by provider and model for the last seven days. Open a run to see its
totals and individual calls, including successful voice transcriptions.
Usage is recorded for new calls as providers supply it. Missing values are
marked **Not reported**.

## Agent memory

Ask your Chief to remember a preference across conversations. You can name
one of your helpers to make the note available to it, or say “remember this
for the group” to share a note with your Chief and all your owned helpers.
Manage each scope under **Settings & forms → Memory**. Invited members retain
access to the notes you explicitly publish for their shared helper.

With your saved OpenRouter key, Jev picks relevant notes for each turn and
checks whether a new Telegram message contains a lasting fact or preference.
TinyBot saves a short private note when appropriate and can retire older
automatic notes when you correct them. Explicitly saved notes stay available
until you remove them.

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

TinyBot explains requested MCP calls in Telegram. Tap **Approve once**,
**Always approve**, or **Deny**. Always approve applies to that exact tool on
the current connection. Revoke it in **Settings & forms → Connections**.
Replacing or removing a connection clears its saved approvals.

### Self-hosted AnythingMCP

Run the gateway as a separate Dokploy Docker Compose service using
[`compose.anythingmcp.dokploy.yaml`](compose.anythingmcp.dokploy.yaml). It uses
the pinned upstream image (`linux/amd64`) and a private PostgreSQL database.

| Variable | Value |
| --- | --- |
| `AMCP_DOMAIN` | Your gateway hostname, without a scheme or port |
| `AMCP_JWT_SECRET`, `AMCP_COOKIE_SECRET`, `AMCP_POSTGRES_PASSWORD` | Run `openssl rand -hex 32` separately for each secret |
| `AMCP_ENCRYPTION_KEY` | Run `openssl rand -hex 16` to generate the 32-character key upstream expects |

Back up the database volume and keep the secrets across redeployments. Start
the stack with access restricted to you while you register the first admin.
In Dokploy, route the gateway hostname to `gateway` on port `8080`, with
path `/` and HTTPS. The proxy connects the dashboard and OAuth callbacks
to their upstream routes. The first account becomes admin. Keep the route private
or IP-restricted until registration is complete. The dashboard lets you skip
email verification and license setup on a self-hosted instance. The stack
blocks the vendor mail relay and license hostnames. To enable verification
and invitations through your own mail server, set `AMCP_SMTP_HOST`,
`AMCP_SMTP_USER`, `AMCP_SMTP_PASS`, and `AMCP_SMTP_FROM`. Set `AMCP_SMTP_PORT`
if it differs from 587 and `AMCP_SMTP_SECURE=true` for implicit TLS.

In your gateway, create an MCP server and copy its HTTPS endpoint, including
the full `/mcp` or `/mcp/<server-id>` path. In TinyBot, open **Connections →
Connect AnythingMCP** and paste it. Sign in to your gateway with OAuth, or
use a per-user MCP API key created in the gateway. **View tools** lists the
tools TinyBot discovers for that account. TinyBot applies the same tool permissions,
Telegram approvals, and audit logging as for other MCP connections.

With your saved OpenRouter key, Jev can confirm that a call uses an
operator-verified tool to read data. Writes and ambiguous calls require a
decision unless you have explicitly remembered approval for that tool. The
bundled Compose policy lists verified Docblock read tools. Add other reviewed
endpoint and tool pairs to the JSON policy to allow smart approval for them.
To delegate a connector read to a shared helper, choose the verified tool
and exact JSON arguments in the helper’s Sharing settings. Scope the connector’s
upstream credentials to the data you intend to share.

## Chief and helpers

All Telegram conversations are handled by your Chief. Manage saved helpers
under **Settings & forms → Helpers**. The Chief chooses a suitable helper,
sends a brief update when it delegates work, and follows up with the result.
If someone shares a helper with you, ask the Chief to consult it by name.
Access to the shared helper's records follows the owner's read permissions.

Chief and its helpers share a hard budget of 100 tool attempts per turn,
including after an approval pause. With a saved OpenRouter key, Jev checks
for repeating work every 10 attempts. When tool work stops, Chief answers
from the results it has and explains any missing information.

## Tasks and schedules

- Reply to an earlier TinyBot message to give the Chief that message as
  context, including when you send a voice note. The Chief answers in the
  reply thread. You can send an emoji as a reply in private chat. Telegram
  only sends native reaction updates when its Bot API permission rules allow it.
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
