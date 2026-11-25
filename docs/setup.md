# Set up TinyBot

TinyBot needs a Linux Docker host with Compose v2, a Telegram bot token, and
a public HTTPS hostname. Both deployment options run TinyBot with its
browser, isolated Python worker, and bundled AnythingMCP gateway. Run one
TinyBot replica per SQLite database. The gateway uses a pinned `linux/amd64`
image.

## Deploy with Docker Compose

1. Create a bot with [BotFather](https://t.me/BotFather). Keep its token and
   your numeric Telegram user ID handy.
2. Point your bot hostname at the host. Keep ports 80 and 443 reachable so
   Caddy can serve HTTPS. Choose a [hostname for AnythingMCP](#anythingmcp-and-cloudflare)
   and create its DNS record on the same host if you plan to use the gateway.
3. For a fresh install, prepare your configuration and secret files from the
   repository root. For an existing install, reuse the saved files and key.

   ```sh
   cp .env.example .env
   umask 077
   mkdir -p secrets
   openssl rand -hex 32 > secrets/encryption_key
   ```

   Put the bot token in `secrets/telegram_bot_token`. Set `TINYBOT_DOMAIN` to
   your hostname without a scheme, and set `TELEGRAM_OPERATOR_ID` to your
   numeric Telegram user ID in `.env`. The TinyBot container runs as UID 1000.
   Give it read access to both secret files and restrict access on the host.
4. Start the stack, then check the public health endpoint with your hostname.

   ```sh
   docker compose up -d --build
   docker compose ps
   curl -fsS https://bot.example.com/health
   ```

Keep the original encryption key across upgrades. The backup section explains
how to restore the data that it protects.

## Deploy with Dokploy

Create a **Docker Compose** service from the repository and select
[`compose.dokploy.yaml`](../compose.dokploy.yaml). Set these values in
**Environment**.

| Variable | Value |
| --- | --- |
| `TINYBOT_DOMAIN` | Public hostname, for example `bot.example.com` |
| `TELEGRAM_OPERATOR_ID` | Your numeric Telegram user ID |
| `TELEGRAM_BOT_TOKEN` | The token from BotFather |
| `TINYBOT_ENCRYPTION_KEY` | Generate 64 hexadecimal characters with `openssl rand -hex 32` |
| `AMCP_DOMAIN` | Optional gateway hostname, for example `mcp.example.com` |

In **Domains**, route the TinyBot hostname to service `tinybot` on container
port `8080` with path `/` and HTTPS enabled. The Compose file joins
`dokploy-network` so Traefik can terminate TLS. Deploy and check
`https://bot.example.com/health` with your hostname. Dokploy passes the bot
token and encryption key in the container environment, which Docker admins
can inspect. Store a protected copy of the key for restores.

The bundled gateway has its own Traefik HTTPS route. Give the chosen gateway
hostname a public DNS record before opening its dashboard.

## Open the bot and choose a model

1. In Telegram, send `/start` to the bot in a private chat. Send `/settings`
   and tap **Settings & forms**. `/panel` shows the available chat controls.
2. For a shortcut from the bot profile, set the Main Mini App URL in BotFather
   to the bot's HTTPS origin or its `/mini` path. Telegram signs people in
   when they open the Mini App from the bot. TinyBot admits the operator and
   invited users. [Telegram Mini App setup](https://core.telegram.org/bots/webapps#launching-the-main-mini-app)
3. On the first launch, enter your name, your Chief's name, and an OpenRouter
   API key. Choose a tool-capable task model from the catalog, then finish
   setup. Change the active provider or model later under **Model**.
4. Send `/invite` to invite another user. For access to a specific read-only
   helper, create an invitation in that helper's **Sharing** settings.

The OpenRouter key also enables Mercury 3 for forms
when Codex is the active task model. **Model → OpenRouter tools** controls the
hosted web search, web fetch, date and time, and model search tools.

### Pair ChatGPT for Codex

With an eligible ChatGPT plan, open **Model → ChatGPT for Codex** in the Mini
App. Enter a Codex model ID and choose **Create pairing code**. From the
repository checkout on a computer with a browser, build TinyBot and run the
command using the bot's public HTTPS origin.

```sh
cargo build --locked --release
./target/release/tinybot codex connect https://bot.example.com
```

Enter the code at the prompt and complete the browser sign-in. If you already
paired Codex, choose **Use Codex for tasks** to make it the active provider.

## AnythingMCP and Cloudflare

The main stacks start AnythingMCP automatically. They generate its
credentials, store them in persistent volumes, and run PostgreSQL on a
private network. The gateway uses `mcp.<TINYBOT_DOMAIN>` by default. Set
`AMCP_DOMAIN` in `.env` or Dokploy **Environment** to choose another hostname.
Caddy and Dokploy both use that value for routing.

On a full Cloudflare zone, Universal SSL covers the apex and first-level
subdomains at the proxy edge. A simple proxied setup uses
`TINYBOT_DOMAIN=bot.example.com` and `AMCP_DOMAIN=mcp.example.com`. If you keep
the nested default `mcp.bot.example.com`, provision a Cloudflare edge
certificate that covers it, or set that record to **DNS only** and have
Caddy or Dokploy serve a publicly trusted certificate for it.
[Cloudflare certificate coverage](https://developers.cloudflare.com/ssl/edge-certificates/universal-ssl/limitations/#hostname-coverage)

### Create the first gateway admin

1. Retrieve the private setup password from the server. For Dokploy, add
   `-f compose.dokploy.yaml` after `docker compose` or run the `cat` command
   in the `anythingmcp` service terminal.

   ```sh
   docker compose exec -T anythingmcp cat /run/anythingmcp-secrets/setup_password
   ```

2. Open `https://<gateway-hostname>/setup`. Sign in with username `setup`
   and the generated password. Create the first admin account in the gateway
   form. The self-hosted dashboard lets you skip email verification and
   license activation.
3. In TinyBot, open **Connections → Connect AnythingMCP**. The bundled
   gateway URL is already filled in. Paste a full `/mcp/<server-id>` URL if
   you want a specific gateway server. Finish with OAuth or a personal MCP
   API key. **View tools** lists the tools available to your account.

For standalone deployment, use
[`compose.anythingmcp.dokploy.yaml`](../compose.anythingmcp.dokploy.yaml) in a
separate project, set `AMCP_DOMAIN`, and route the hostname to `gateway:8080`
in Dokploy Domains. In TinyBot's Connections tab, enter the standalone
gateway URL manually.

The gateway blocks its upstream mail relay and license hosts. To send
gateway verification or invitation emails through your own mail server, set
`AMCP_SMTP_HOST`, `AMCP_SMTP_USER`, `AMCP_SMTP_PASS`, and `AMCP_SMTP_FROM`.
The SMTP port defaults to 587. Use `AMCP_SMTP_PORT` to override it and set
`AMCP_SMTP_SECURE=true` for implicit TLS. These settings apply to gateway
account mail. Configure mail for your Chief separately under **Email**.

## Configure optional services

Use `TINYBOT_TIMEZONE` to set the deployment's default time zone, such as
`Europe/Amsterdam`. Each user can select a time zone under **Schedules**.

### Email for your Chief

Open **Email** in the Mini App. Enter your provider's public IMAP or SMTP
hostnames and credentials. IMAP uses port 993 over TLS. SMTP uses port 465
over TLS or port 587 with required STARTTLS. Add the mailbox folders you want
to monitor and choose any automation rules per folder. [Email workflows](features.md#follow-up-on-email)

### Browser proxy and search

As the operator, open **Browser** in the Mini App to save a proxy, use a
direct connection, or restore the deployment setting. Set
`TINYBOT_BROWSER_PROXY` in `.env` or Dokploy to configure a deployment default.
An empty deployment setting uses direct connections.

| Proxy | URL format | Port |
| --- | --- | --- |
| SOCKS5 | `socks5://[username:password@]host:port` | Required |
| HTTP CONNECT | `http://[username:password@]host:port` | Defaults to 80 |

Use a public proxy endpoint. Credentials are optional, and HTTP proxy
authentication uses Basic. Percent-encode reserved characters in credentials,
such as `@` as `%40`. The proxy applies to new browser connections and
browser-backed searches. Browser traffic uses the configured proxy
exclusively and reports proxy failures. A `TINYBOT_BRAVE_SEARCH_KEY` enables
the Brave Search API on a separate connection.

### Action review and MCP policy

Action review defaults to a separate call to the active task model. Set
`TINYBOT_REVIEW_MODEL=typesafe/jev-1.14` to use the typed Decisions API on
OpenRouter. `TINYBOT_REVIEW_OPENROUTER_KEY` is an optional deployment fallback
for action review and Mercury. Personal OpenRouter credentials take
precedence.

### Telegram progress notices

When a private Telegram request is still open after five seconds, TinyBot can
generate a short update using the topic and any confirmed work. Set
`TINYBOT_INCEPTION_API_KEY` to use Mercury 3 directly for these notices. If the
key is absent or the Mercury call fails, TinyBot generates the notice with the
user's configured task model. Inception plans to retire this API after
December 28, 2026. [Inception API wind-down](https://docs.inceptionlabs.ai/support/wind-down).

### MCP read policy

Remote MCP servers can use Streamable HTTP or legacy HTTP+SSE. Depending on
the server, authorization can use OAuth client metadata discovery, dynamic
registration, a supplied OAuth client, or an API token with a configurable
header and prefix. For a supplied OAuth client, register
`https://bot.example.com/oauth/mcp` with your own hostname as the callback.

The Compose policy lists verified Docblock read tools. You can point
`TINYBOT_MCP_READ_POLICY_PATH` to a JSON map with reviewed connector URLs and
exact read-tool names. Use the [bundled policy](../deploy/mcp-reads.json) as a
reference. Helper sharing adds a separate grant for each verified tool and its
exact arguments.

#### Local MCP programs

Place operator-reviewed programs in the ignored `mcp-programs/` directory.
List the allowed commands in `mcp-programs/programs.json`. For example,
this entry provisions `example.py` as a Python MCP server.

```json
{
  "example": {
    "command": ["/usr/local/bin/python", "/programs/example.py"],
    "credential_env": "EXAMPLE_API_TOKEN"
  }
}
```

Run `docker compose --profile stdio up -d --build` and ask the Chief to
connect `stdio://example`. The local worker runs provisioned commands
offline. For a connector that requires network access, use a public HTTPS
MCP endpoint.

## Back up and restore

Take a SQLite snapshot from the repository's Compose project. Choose a fresh
destination for each snapshot so the backup never overwrites a previous copy.

```sh
docker compose exec tinybot tinybot backup /data/snapshot.sqlite
docker compose exec tinybot tinybot check-backup /data/snapshot.sqlite
docker compose cp tinybot:/data/snapshot.sqlite ./snapshot.sqlite
```

Keep the key in a secure backup separate from the database file. To restore,
stop TinyBot, place a copy of the snapshot at `/data/tinybot.sqlite` in a
clean application volume, and start TinyBot with the original encryption key.
Restoring that key lets TinyBot decrypt stored provider and connector secrets.
If the original key is lost, reconnect the affected accounts.

Back up the AnythingMCP PostgreSQL data and generated secret volumes if you
use the gateway. Restore them together to retain gateway accounts and access.

## Verify a local change

```sh
cargo fmt --check
cargo test --locked
docker compose config --quiet
docker compose -f compose.dokploy.yaml config --quiet
```

Supply the required deployment variables when checking either Compose file.
The fixtures in [`tests/`](../tests) exercise Mini App rendering, sandboxed
Chromium forms, the Python worker, and an operator-configured stdio MCP.
Mount `tests/browser.test.mjs` at `/opt/tinybot/browser.test.mjs` in the
browser image and use the Compose seccomp settings for Chromium. Check Python
file mutation, job isolation, and resource limits with
`docker compose exec -T sandbox python - < tests/python_worker.py`. Live
Telegram, OAuth, and model-provider checks need accounts and a public hostname.

## Verify a rollout

Build the core, browser, and sandbox with `TINYBOT_RELEASE_SHA` set to the full
commit SHA of the checkout. In Dokploy, set that value on the Compose project
before building the release.

```sh
export TINYBOT_RELEASE_SHA="$(git rev-parse HEAD)"
docker compose build tinybot browser sandbox
python3 deploy/verify-release.py https://bot.example.com "$TINYBOT_RELEASE_SHA"
```

Run the verification command after the new stack starts. It checks the database,
both worker revisions, and the served Mini App files against the checkout. The
same command works against an isolated HTTP stack on loopback.
