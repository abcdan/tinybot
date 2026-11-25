FROM mcr.microsoft.com/playwright:v1.58.2-noble
ARG TINYBOT_RELEASE_SHA
ENV TINYBOT_RELEASE_SHA=${TINYBOT_RELEASE_SHA}
WORKDIR /opt/tinybot
RUN apt-get update && apt-get install -y --no-install-recommends ffmpeg \
    && rm -rf /var/lib/apt/lists/* \
    && npm install --no-audit --no-fund --ignore-scripts --save-exact playwright@1.58.2 \
    && mkdir -p /browser \
    && chown 1000:1000 /browser
COPY server.mjs forms.mjs capture.mjs ./
USER 1000:1000
CMD ["node", "server.mjs"]
