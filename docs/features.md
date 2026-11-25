# What TinyBot can do

Send TinyBot a message in Telegram to start a task. Your Chief handles the conversation,
asks for missing details, and comes back to you when it has a result or needs a
decision. Use `/settings` for the Mini App and `/panel` for the controls you can open
from chat.

Under **Model**, choose a tool-capable OpenRouter model or pair an eligible ChatGPT
account for Codex. A saved OpenRouter key also enables voice transcription and Mercury 3
forms when Codex handles tasks. [Connect a model](setup.md).

With both providers connected, you can choose a **Backup model**. TinyBot can make one
attempt with that provider after a temporary model failure before tool work starts.
The chat shows when the task switches providers.

## Take a task beyond one message

> Compare these three options and tell me which is easiest to set up.

- Follow up in chat to steer the current task. Reply to an earlier bot message to pick
  up that conversation and give the Chief the quoted message as context.
- Use `/new` to start fresh, `/tasks` to see recent tasks, `/switch ID` to select one
  again, or `/cancel` to stop the current task. Send `/status` to check the current
  task, or reply with `/status` to check an earlier reply. Use **Continue**, **Stop**,
  or **Review** when TinyBot shows that action. TinyBot keeps tracking an approved
  action until it can confirm the outcome or asks you to check the destination.
- Tell TinyBot when a task is done, use `/done`, or tap **Mark complete** in
  **Tasks**. Completed tasks move out of the open list. Find them in **Completed
  tasks** or through search, and reopen them to pick up the conversation again.
- Quick questions go straight to the answer. If a private task is still queued
  or running after a short wait, TinyBot can generate a brief update in your
  language. An answer, draft, or helper handoff can take the place of that update.
  Use **Tasks** on the Mini App home screen to search titles, summaries, and retained
  chat messages.
- When a model reaches its response limit on a private task, TinyBot tries a
  smaller next step or finishes with the results it already has. Long replies
  can arrive in multiple Telegram messages. If recovery fails, TinyBot uses the
  audit log to explain the last confirmed step and what it tried. If the request can
  safely run again before any tools or actions started, tap **Retry** or send
  `/retry` to retry the saved message. If TinyBot asks you to reconnect or choose
  a model, you can send `/retry` after updating **Settings & forms → Model**.
- If TinyBot says it lost confirmation of an external action, check the destination and
  send `/reconcile` to continue. TinyBot holds uncertain work so it can resume without
  automatically repeating the action.
- For longer work, TinyBot can show a typing indicator or stream a draft as it forms.
  Final replies can include suggested
  responses, source buttons, formatted comparisons, and cards. Telegram renders native
  widgets where available, with formatted messages and buttons as a fallback. Send an
  emoji as a reply in private chat. Native reaction updates depend on Telegram’s Bot API
  permissions.

## Make a specialist available

> Ask Researcher to check the publication dates and bring back the original sources.

Create **Researcher** under **Helpers**, give it instructions, and select the tools it
can use. The Chief can choose a suitable helper on its own or consult one you name. The
Chief sends a brief handoff update and returns the result in your conversation.

Use a helper’s **Sharing** settings to issue a one-time invitation, publish notes, or
grant a verified connector read with fixed arguments. Invited members ask their own
Chiefs to consult the shared helper. Published notes and permitted records can be sent
to the invited member’s model provider.

The Chief and its helpers share a limit of 100 tool attempts per turn, including after
an approval pause. With an OpenRouter key, Jev checks for repeating work every 10
attempts. If tool work stops, the Chief works from the results it has and can tell you
what remains open.

## Share work in a project

Open **Memory → Project spaces** to create a project. Invite existing TinyBot
users by their Telegram account ID. Each person reviews and accepts their role.
Editors can publish notes or copies of their private task results. Viewers can read
items shared with them and check off items assigned to them. Owners manage
membership and assignments.

Review each snapshot’s version and audience before publishing. New members only
receive access to items published for them. Revoking a member removes their
access to the project and any existing assignments. Ordinary chats and memories stay
private. Use **Shared memory** for a separate paired chat and published notes.

## Research the public web

> Check this public page and compare the pricing with two official sources.

Send a public HTTPS link or a search request. The Chief can browse pages, look for
relevant sources, and give you links to open in Telegram. A site can still require
verification before its contents are available.

Under **Model → OpenRouter tools**, toggle **Web search**, **Web fetch**, **Date and
time**, and **Search models**. These options are on by default when OpenRouter is the
active task provider. For the local browser, operators can configure direct access or a
public SOCKS5 or HTTP CONNECT proxy in **Browser**. Configuring a proxy routes local
browser traffic through it and disables hosted Web search and Web fetch. A configured
Brave Search API key uses a separate connection. See [setup](setup.md) for proxy and
search settings.

## Take a voice note or read a file

> I’ve attached the minutes. Pull out decisions and give me three action items.

| What you can send in private chat | What TinyBot does |
| --- | --- |
| Voice notes up to 10 MB | Sends the audio to OpenRouter for transcription using your saved key. The transcript enters the task as a `Voice message:` even when Codex handles the task. |
| JPEG, PNG, or WebP up to 8 MB | Passes the image to a vision-capable task model. Add a question in the caption or send it as a follow-up. |
| PDFs up to 8 MB | Uses OpenRouter’s file input or extracts text locally from the first 12 pages for Codex. |
| UTF-8 text up to 8 MB | Provides text from files such as CSV and Markdown for reading or summarizing. |

Reply directly to an earlier upload to use it again in the same task. TinyBot encrypts
stored attachments. The Chief can also use offline Python on its task files for
calculations and text processing. Ask for a CSV, Markdown, JSON, or text file to
receive a Telegram document up to 32 KiB.

In a private chat, ask **Can I get that as audio?** to hear the latest TinyBot
answer. To hear a specific answer, reply to it with that question or with 🙉.
A 🙉 reaction also works when Telegram delivers reaction updates to the bot.
TinyBot sends the selected answer to OpenRouter using your saved key for speech
synthesis.

## Ask TinyBot about its settings

> What model are you using? Change your name to Atlas and set my timezone to Europe/Amsterdam.

In a private chat, your Chief can read your personal settings and see which
services are connected. When you ask for a change, TinyBot opens a form with the
requested fields. Submit the form to compare the current and proposed values,
then tap **Approve changes** to save them. If a setting changed since you opened
the form, ask for a fresh one.

You can change your display name, your Chief’s name, your timezone, your models
from already-connected providers, and your OpenRouter tool toggles. Use the
existing **Settings & forms** screens to connect accounts or manage credentials
and access.

## Collect details and review actions

> Ask me for the dates and preferred format, then draft the agenda.

A simple choice can appear as buttons in Telegram. For several fields, tap **Open form**
to enter your answers in the Mini App. The Chief picks up the task after you submit. It
can use your task model or Mercury 3 to prepare the form.

For a supported public website form, TinyBot can offer private inputs in the Mini App
and ask you to review a masked preview before submitting. It submits to the same website
over HTTPS. The website form values go to TinyBot over HTTPS and stay in memory until
use or expiry. Use provider or connector settings for API keys and credentials.

## Run a task again

> Every Monday at 9 am, remind me to review the weekly plan.

The Chief saves your instruction and opens a new task when it’s due. The result arrives
in Telegram. Open **Schedules** to edit instructions and timing, pause or resume a
schedule, start a run with **Run now**, and reopen a past run from **Run history**.
Recurring schedules use the time zone selected when they were created. After downtime,
they advance from the current time.

Fixed reminders send the saved text directly. Open **Timing policy** to set quiet
hours or decide whether a missed recurring schedule should run once or be skipped.
**Run history** shows delayed and skipped occurrences as well as results. Use **Dry
run** to preview a routine with sample input in Telegram.
For recurring digests, choose **Only when the result changes** under **Timing policy**
to skip notifications when the facts and next actions match the last delivered result.

Open **Webhook** on an active or manual routine to run it from a trusted external
service. The service sends `POST` with a JSON body containing a unique `event_id`.
Send the current Unix timestamp in seconds as `X-TinyBot-Timestamp` and the hex
HMAC-SHA256 of `timestamp + "." + raw_body` as `X-TinyBot-Signature`, using the
shown signing key. Repeating an accepted event acknowledges the same run. A busy
routine returns `409` so the sender can retry. Changing the routine requires a new
webhook. Existing approvals still apply to the triggered run.

Ask the Chief to save a repeatable procedure, then review its steps before saving.
Choose whether each step runs with the Chief or one of your own helpers. A run uses
the steps saved when it started and asks for approvals through the normal task flow.
Start with **Inbox briefing** for messages in folders enabled for automatic handling,
or edit **Weekly research digest** to follow a topic using public sources.

## Remember a preference

> Remember that I like agendas with decisions first.

The Chief saves explicit requests in its private memory by default. Name a helper to
save a note for it, or say **remember this for the group** to share it with your Chief
and owned helpers. Open **Memory** to see and edit each scope. Shared helpers expose
notes you publish through **Sharing**.

With a saved OpenRouter key, Jev can pick relevant notes, detect lasting preferences,
and retire earlier automatic notes when you correct them. Explicitly saved notes stay
until you remove them.

## Invite someone

The bot operator can send `/invite` to get a shareable Telegram link or create an
invitation under **Invites** to copy a sign-up command. Each invitation works once
and expires after 24 hours. The guest accepts in Telegram and sets up their own
Chief, model, and connections. The operator can revoke unused invitations, suspend
guest access, or restore it.

Before accepting, guests see what the operator can view. **Invites** shows setup
status, the last active day, counts of runs started and completed, and provider-reported
tokens and costs for the past seven days. Conversations, notes, connections, and
individual audit runs stay private. Usage is available for guests who accepted
an invitation with this disclosure.

The operator can offer a guest a shared memory space. Once both people have joined,
each can publish and remove their own notes under **Memory → Shared memory**.
Use **Start shared chat** to work with those notes in a dedicated conversation.
Either person can leave the space.

## Connect an external service

> Connect the Atlassian MCP. Then help me find the planning document.

MCP lets a connected service expose tools to TinyBot. Your Chief can search for a named
connector and show the endpoint for you to review. Open **Connections** to set up a
connector and authorize it using the options that service supports, such as OAuth or a
personal API key. The bundled AnythingMCP gateway can bring compatible APIs and
databases into one connection. Open its card and choose **View tools** to see what
TinyBot discovered. [Set up connections](setup.md).

When a tool call needs a decision, TinyBot explains what it would do and offers
**Approve once**, **Always approve**, and **Deny**. A saved approval covers that tool on
the current connection. Manage these approvals in **Connections**. Replacing or removing
the connection clears its saved approvals. With an OpenRouter key, Jev can check whether
a call uses an operator-verified tool for a read. Writes and ambiguous calls require
approval unless you have explicitly saved approval for that tool.

## Write and share notes in Docblock

> Save these meeting decisions as a Markdown note in my project workspace.

Open **Connections → Connect Docblock**, sign in, and choose the workspaces and
permissions to grant. A task waiting for the connection can continue after sign-in.
Ask the Chief to find, read, write, or append to a note, then review changes that
need approval. TinyBot returns a link that follows the note’s workspace permissions.

Ask for a public link when someone outside the workspace needs access. Review the
exact note and expiry before approving publication. The default expiry is seven days.
Anyone with the link can read the current note until the link expires or is revoked.
Use **View public note links** on the Docblock connection to manage links.

## Follow up on email

> Draft a reply saying Friday afternoon works.

Open **Email** to add IMAP, SMTP, or both. For incoming mail, choose which folders to
watch. IMAP starts at the current mailbox position and watches for new messages. A
folder starts in **Notify only** mode, which hides the sender, subject, and body in
Telegram. Tap **Read more** or reply to the notice to give the Chief the email text. You
can enable automatic handling for a folder to summarize incoming mail, ask you a
question, or prepare a draft.

Every SMTP send asks for one-time approval. Review the sender, recipients, subject, and
body. Choose **Edit email draft** in the Mini App to save changes for a fresh review,
then tap **Approve once** or **Deny**. TinyBot reports when the mail server
accepts the message for delivery. It encrypts mail credentials and retrieved mail.

## Use TinyBot in a Telegram group

In the group pilot, an administrator with a TinyBot account sends
`/tinybot_enable@BotName` in the group. The bot operator sends
`/approve_group CHAT_ID` in private chat after receiving the request. To disable
group access, the operator sends `/disable_group CHAT_ID` in private chat.

Members with a configured TinyBot model can send `/ask@BotName` followed by a drafting
or public research request. Reply to your own task’s result in the same group and
topic to continue it. Results are visible in that group. Personal memories, mail,
and connections are available in private chat only.

## See how a run unfolded

Open **Audit** in the Mini App to see model calls, tool calls, review decisions, and
outcome summaries for the past seven days. Open a run for its activity and reported
token usage and costs. **Usage breakdown** groups reported values by provider and model.
Missing usage is marked **Not reported**.

Choose **Export JSON** to download the full retained run, including entries beyond the
first page and its usage totals. **Copy JSON** is available as a fallback. The export
uses the same redacted data as the audit view. It omits credentials, private form
values, and raw model prompts.

For installation and account setup, see [Setup](setup.md). For the path a request takes
through TinyBot, see [How TinyBot works](architecture.md).
