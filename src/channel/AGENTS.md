# Channel adapters

- Keep Telegram JSON parsing and response formatting in the Telegram adapter. Put update handling, retries, outbox state transitions, and task ingress in the dispatcher.
- Telegram update IDs deduplicate events. Telegram message IDs identify reply and reaction targets. Scope a message ID to its chat before using it to find a task.
- Preserve the existing private-chat and sender checks for messages, widget events, callbacks, and media. Treat quoted text and remote file metadata as untrusted data.
- Use the mock Bot API fixtures to verify payloads. Cover explicit API rejections and retry delays when changing delivery behavior.
