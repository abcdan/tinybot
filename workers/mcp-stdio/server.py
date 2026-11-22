"""Bridge operator-provisioned MCP programs to TinyBot over a private Unix socket."""
import http.server
import json
import os
from pathlib import Path
import re
import selectors
import signal
import socketserver
import subprocess
import time

SOCKET = Path("/mcp-stdio/stdio.sock")
CONFIG = Path("/programs/programs.json")
NAME = re.compile(r"[a-zA-Z0-9_-]{1,64}")
ENV = re.compile(r"[A-Z][A-Z0-9_]{0,63}")
LIMIT = 512 * 1024


def parse_program(alias):
    if not isinstance(alias, str) or not NAME.fullmatch(alias):
        raise ValueError("Invalid stdio program.")
    definition = json.loads(CONFIG.read_text())[alias]
    command = definition["command"]
    if not isinstance(command, list) or not command or not all(isinstance(arg, str) for arg in command):
        raise ValueError("Invalid registered command.")
    # The operator may choose a runtime such as /usr/local/bin/python with a
    # fixed script in /programs, or mount a standalone executable.
    if not command[0].startswith(("/programs/", "/usr/local/bin/")):
        raise ValueError("Invalid registered executable.")
    return command, definition.get("credential_env")


def exchange(data):
    command, token_env = parse_program(data["alias"])
    method = data["method"]
    if method not in ("tools/list", "tools/call") or not isinstance(data["params"], dict):
        raise ValueError("Invalid MCP method.")
    environment = {"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": "/tmp", "PYTHONDONTWRITEBYTECODE": "1"}
    credential = data.get("credential")
    if credential is not None:
        if not isinstance(token_env, str) or not ENV.fullmatch(token_env) or len(credential) > 8192:
            raise ValueError("The program does not accept a credential.")
        if token_env in ("PATH", "HOME") or token_env.startswith(("PYTHON", "LD_", "DYLD_")):
            raise ValueError("The credential environment name is reserved.")
        environment[token_env] = credential
    with subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                          env=environment, cwd="/tmp", start_new_session=True) as child:
        try:
            return session(child, method, data["params"])
        finally:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass


def session(child, method, params):
    deadline = time.monotonic() + 22
    read = bytearray()
    consumed = 0
    with selectors.DefaultSelector() as poller:
        poller.register(child.stdout, selectors.EVENT_READ)

        def request(sequence, method, params):
            payload = {"jsonrpc": "2.0", "method": method, "params": params}
            if sequence:
                payload["id"] = sequence
            child.stdin.write(json.dumps(payload).encode() + b"\n")
            child.stdin.flush()
            return response(sequence) if sequence else None

        def response(sequence):
            nonlocal consumed
            while True:
                index = read.find(b"\n")
                if index >= 0:
                    message = json.loads(read[:index])
                    del read[:index + 1]
                    if message.get("id") != sequence:
                        continue
                    if "error" in message or "result" not in message:
                        raise ValueError("MCP program rejected the request.")
                    return message["result"]
                wait = deadline - time.monotonic()
                if wait <= 0 or not poller.select(wait):
                    raise TimeoutError
                chunk = os.read(child.stdout.fileno(), 8192)
                if not chunk or consumed + len(chunk) > LIMIT:
                    raise ValueError("Invalid MCP program response.")
                consumed += len(chunk)
                read.extend(chunk)

        version = request(1, "initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                                             "clientInfo": {"name": "tinybot", "version": "0.1"}})
        if version.get("protocolVersion") not in ("2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"):
            raise ValueError("Unsupported stdio MCP protocol.")
        request(None, "notifications/initialized", {})
        if method == "tools/list":
            tools = []
            for number in range(2, 6):
                page = request(number, method, params)
                tools.extend(page["tools"][:250 - len(tools)])
                cursor = page.get("nextCursor")
                if not cursor or len(tools) == 250:
                    break
                params = {"cursor": cursor}
            return {"tools": tools}
        return request(2, method, params)


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        try:
            size = int(self.headers.get("Content-Length", "0"))
            if self.path != "/rpc" or not 0 < size <= 96_000:
                raise ValueError("Invalid worker request.")
            result = exchange(json.loads(self.rfile.read(size)))
            self.reply(200, result)
        except (OSError, KeyError, TypeError, ValueError):
            self.reply(422, {"error": "The configured MCP program did not complete the request."})

    def reply(self, status, value):
        body = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


if __name__ == "__main__":
    SOCKET.unlink(missing_ok=True)
    with socketserver.UnixStreamServer(str(SOCKET), Handler) as server:
        SOCKET.chmod(0o660)
        server.serve_forever()
