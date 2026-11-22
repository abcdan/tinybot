FROM rust:1.100-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY web ./web
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --locked --release && cp /src/target/release/tinybot /usr/local/bin/tinybot

FROM node:24-bookworm-slim AS core
ARG CODEX_VERSION=0.171.0
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && npm install --global --omit=dev "@openai/codex@${CODEX_VERSION}" \
    && mkdir -p /data /browser /sandbox /mcp-stdio \
    && chown node:node /data /browser /sandbox /mcp-stdio
COPY --from=build /usr/local/bin/tinybot /usr/local/bin/tinybot
USER node
ENTRYPOINT ["tinybot"]
CMD ["serve"]

FROM debian:bookworm-slim AS egress
COPY --from=build /usr/local/bin/tinybot /usr/local/bin/tinybot
USER 1000:1000
ENTRYPOINT ["tinybot", "egress"]
