# Model providers

- Keep database and vault operations in `connections.rs`. Put OpenRouter key
  verification and catalog requests in `connections/catalog.rs`, with bounded
  HTTPS responses via `crate::net`. Preserve the user-scoped encryption bindings,
  legacy key migration, and compare-and-swap when choosing a task model.
- Use the saved user OpenRouter key for task turns and voice transcription.
  Deployment fallback keys are scoped to the existing Mercury and review paths.
  Keep credentials and raw provider responses out of logs.
- Check protocol changes against the Codex App Server version pinned in the
  Dockerfile and the OpenRouter adapters. Register only broker-provided tools.
  Hand valid native calls to the runtime for authorization and execution. Stream
  only reply text and keep tool arguments and reasoning out of chat previews.
