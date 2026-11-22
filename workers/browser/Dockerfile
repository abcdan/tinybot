FROM mcr.microsoft.com/playwright:v1.58.2-noble
WORKDIR /opt/tinybot
RUN npm install --no-audit --no-fund --ignore-scripts --save-exact playwright@1.58.2 \
    && mkdir -p /browser \
    && chown 1000:1000 /browser
COPY server.mjs forms.mjs ./
USER 1000:1000
CMD ["node", "server.mjs"]
