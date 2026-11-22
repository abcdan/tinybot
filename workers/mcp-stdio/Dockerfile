FROM python:3.14-slim-bookworm
RUN mkdir -p /mcp-stdio /programs && chown 1000:1000 /mcp-stdio
COPY server.py /opt/tinybot/server.py
USER 1000:1000
CMD ["python", "/opt/tinybot/server.py"]
