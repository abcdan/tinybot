# Mini App client

- Use Telegram theme variables with light and dark fallbacks. Keep focused forms usable at narrow widths.
- Use the common controls in `mini-ui.js` and the signed API client in `mini.js`. The server owns permissions and request validation.
- Render user and provider strings as text. Use Telegram's link handler for explicit external navigation.
- Verify changed flows with `tests/mini_app_render.test.mjs` in the Chromium test environment.
