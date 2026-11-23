# TinyBot contributing guide

## Architecture

- `src/channel.rs` defines inbound events and the channel adapter contract. The Telegram adapter translates Bot API payloads. The dispatcher handles ingress and sends durable outbox items.
- `src/store/` owns task revisions, permissions, approvals, and the durable queue. Add numbered SQLite migrations for schema changes and keep earlier migrations intact.
- `src/runtime/` runs the Chief and its helpers. It interprets model proposals, checks tool permissions, and records actions before the channel delivers a response.
- `src/model/` handles provider protocols and model connections. Provider output proposes actions for the runtime broker.
- `src/web/` serves and authorizes the Mini App API. `web/` is the client. Workers in `workers/` run independently of the API process.

## Changes and verification

- Exercise the normal path for the feature you changed. Keep task revision checks, shared-helper grants, and outbox delivery checks in place when moving code.
- Keep credentials, private form values, and raw provider payloads out of audit records. Work with synthetic keys and local service fixtures.
- Run `cargo fmt --check` and `cargo test --locked`. Add focused coverage when updating a protocol or a persistent workflow.
- Use the Chromium fixtures in `tests/` for Mini App and browser changes. Validate both Compose entry points after deployment configuration changes.
