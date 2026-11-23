# Web routes

- Keep route registration and shared HTTP security headers in `mod.rs`. `mini.rs` owns the signed `/api/mini` envelope, action routing, and client error responses. Put action-specific validation and service calls in the matching module under `mini/`.
- Validate the request's exact configured `Origin` and verify Telegram init data with `security::telegram_user` before dispatch. Scope lookups and mutations to the authenticated Telegram user and apply the resource's ownership or grant checks.
- Preserve the allowlisted CSP, request size limit, and no-store responses. Keep init data, provider credentials, and private form values out of logs and errors. Use the existing connector service for consent, credential storage, and outbound requests.
- Keep Mini App action names and response fields in sync with `web/mini*.js`. Run `cargo test --locked` after API changes. The signed request fixtures in `src/acceptance.rs` and `src/acceptance/` use synthetic Telegram init data and local services.
