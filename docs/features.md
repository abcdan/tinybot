# What TinyBot can do

Send TinyBot a message in Telegram to start a task. Your Chief handles the conversation,
asks for missing details, and comes back to you when it has a result or needs a
decision. Use `/settings` for the Mini App and `/panel` for the controls you can open
from chat.

Under **Model**, choose a tool-capable OpenRouter model or pair an eligible ChatGPT
account for Codex. A saved OpenRouter key also enables voice transcription and Mercury 3
forms and acknowledgements when Codex handles tasks. [Connect a model](setup.md).

## Take a task beyond one message

> Compare these three options and tell me which is easiest to set up.

- Follow up in chat to steer the current task. Reply to an earlier bot message to pick
  up that conversation and give the Chief the quoted message as context.
- Use `/new` to start fresh, `/tasks` to see recent tasks, `/switch ID` to select one
  again, or `/cancel` to stop the current task. An action already underway can still
  finish after cancellation.
- If TinyBot says it lost confirmation of an external action, check the destination and
  send `/reconcile` to continue. TinyBot holds uncertain work so it can resume without
  automatically repeating the action.
- For longer work, TinyBot can show a typing indicator, stream a draft as it forms, or
  send a short acknowledgement using Mercury 3. Final replies can include suggested
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
calculations and text processing.

## Collect details and review actions

> Ask me for the dates and preferred format, then draft the agenda.

A simple choice can appear as buttons in Telegram. For several fields, tap **Open form**
to enter your answers in the Mini App. The Chief picks up the task after you submit. It
can use your task model or Mercury 3 to prepare the form.

For a supported public website form, TinyBot can offer private inputs in the Mini App
and ask you to review a masked preview before submitting. It submits to the same website
over HTTPS. The website form values go to TinyBot over HTTPS and stay in memory until
use or expiry. Use provider or connector settings for API keys and credentials.

## Run something later

> Every Monday at 9 am, remind me to review the weekly plan.

The Chief saves your instruction and opens a new task when it’s due. The result arrives
in Telegram, and you can reopen the run with `/tasks` and `/switch ID`. Use
**Schedules** to set your time zone and remove upcoming runs. Five-field cron schedules
use the time zone selected when they were created. After downtime, recurring schedules
advance from the current time.

## Remember a preference

> Remember that I like agendas with decisions first.

The Chief saves explicit requests in its private memory by default. Name a helper to
save a note for it, or say **remember this for the group** to share it with your Chief
and owned helpers. Open **Memory** to see and edit each scope. Shared helpers expose
notes you publish through **Sharing**.

With a saved OpenRouter key, Jev can pick relevant notes, detect lasting preferences,
and retire earlier automatic notes when you correct them. Explicitly saved notes stay
until you remove them.

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

## Follow up on email

> Draft a reply saying Friday afternoon works.

Open **Email** to add IMAP, SMTP, or both. For incoming mail, choose which folders to
watch. IMAP starts at the current mailbox position and watches for new messages. A
folder starts in **Notify only** mode, which hides the sender, subject, and body in
Telegram. Tap **Read more** or reply to the notice to give the Chief the email text. You
can enable automatic handling for a folder to summarize incoming mail, ask you a
question, or prepare a draft.

Every SMTP send asks for one-time approval. Review the sender, recipients, subject, and
body, then tap **Approve once** or **Deny**. TinyBot reports when the mail server
accepts the message for delivery. It encrypts mail credentials and retrieved mail.

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
