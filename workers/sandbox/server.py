"""Run CPython WASI jobs through a private Unix socket."""
import http.client
import http.server
import itertools
import json
import os
from pathlib import Path
import re
import resource
import socket
import socketserver
import subprocess
import sys
import tempfile

from wasm_runtime import FILE_LIMIT, JOB_FILE, guest

SOCKET = "/sandbox/sandbox.sock"
JOB_ROOT = "/work"
TOTAL_LIMIT = 262_144
FILE_COUNT = 24
REQUEST_LIMIT = 1_600_000
OUTPUT_LIMIT = 5_000
WALL_SECONDS = 18
FILENAME = re.compile(r"[a-zA-Z0-9_-][a-zA-Z0-9_.-]{0,95}")


def limited_process():
    resource.setrlimit(resource.RLIMIT_CPU, (13, 14))
    resource.setrlimit(resource.RLIMIT_NOFILE, (64, 64))


def validated_files(files):
    if not isinstance(files, dict) or len(files) > FILE_COUNT:
        raise ValueError("Too many task files.")
    checked = {}
    for name, data in files.items():
        if not FILENAME.fullmatch(name) or not isinstance(data, str):
            raise ValueError("Use plain UTF-8 task files with simple filenames.")
        data = data.encode("utf-8")
        if len(data) > FILE_LIMIT:
            raise ValueError("A task file exceeds its size limit.")
        checked[name] = data
    if sum(map(len, checked.values())) > TOTAL_LIMIT:
        raise ValueError("Task files exceed their total size limit.")
    return checked


def collect(workspace):
    output = {}
    with os.scandir(workspace) as entries:
        for child in itertools.islice(entries, FILE_COUNT + 2):
            if child.name == JOB_FILE:
                continue
            if (not FILENAME.fullmatch(child.name) or not child.is_file(follow_symlinks=False)
                    or child.stat(follow_symlinks=False).st_size > FILE_LIMIT):
                raise ValueError("Python produced an unsupported file.")
            with open(child.path, "rb") as source:
                output[child.name] = source.read(FILE_LIMIT + 1).decode("utf-8")
    validated_files(output)
    return output


def run_job(data, *, diagnostics=False):
    if not isinstance(data, dict) or not isinstance(code := data.get("code"), str):
        raise ValueError("Supply Python source code.")
    code = code.encode("utf-8")
    if not code or len(code) > 16_000:
        raise ValueError("Python source exceeds its size limit.")
    files = validated_files(data.get("files"))
    with tempfile.TemporaryDirectory(dir=JOB_ROOT) as workspace:
        path = Path(workspace)
        (path / JOB_FILE).write_bytes(code)
        for name, body in files.items():
            (path / name).write_bytes(body)
        # Keep files backing the captured streams outside the WASI preopen.
        with tempfile.TemporaryFile(dir=JOB_ROOT) as stdout, tempfile.TemporaryFile(dir=JOB_ROOT) as stderr:
            try:
                process = subprocess.run(
                    [sys.executable, __file__, "--guest-self-test" if diagnostics else "--guest", workspace],
                    cwd=workspace, env={}, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr,
                    timeout=WALL_SECONDS, preexec_fn=limited_process,
                )
            except subprocess.TimeoutExpired:
                return {"ok": False, "stdout": "", "stderr": "Python exceeded its wall time limit.", "files": {}}
            stdout.seek(0)
            stderr.seek(0)
            standard = stdout.read(OUTPUT_LIMIT).decode("utf-8", errors="replace")
            errors = stderr.read(OUTPUT_LIMIT).decode("utf-8", errors="replace")
            if process.returncode != 0:
                if diagnostics and not errors:
                    errors = f"Sandbox subprocess exited with status {process.returncode}."
                return {"ok": False, "stdout": standard, "stderr": errors or "Python exceeded an execution limit.", "files": {}}
        return {"ok": True, "stdout": standard, "stderr": errors, "files": collect(workspace)}


class Worker(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def send_json(self, status, result):
        data = json.dumps(result, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path == "/health":
            release = os.environ.get("TINYBOT_RELEASE_SHA", "")
            valid = len(release) == 40 and all(c in "0123456789abcdef" for c in release)
            self.send_json(200, {"ok": True, "release": release if valid else None})
        else:
            self.send_json(404, {"error": "Unknown sandbox operation."})

    def do_POST(self):
        if self.path != "/run":
            self.send_json(404, {"error": "Unknown sandbox operation."})
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if not 0 < length <= REQUEST_LIMIT:
                raise ValueError("Invalid sandbox request size.")
            data = json.loads(self.rfile.read(length))
            result = run_job(data)
        except (ValueError, UnicodeError):
            self.send_json(422, {"error": "Invalid Python source or task files."})
        except OSError:
            self.send_json(503, {"error": "The Python worker could not complete this job."})
        else:
            self.send_json(200, result)


class Server(socketserver.UnixStreamServer):
    def get_request(self):
        connection, address = super().get_request()
        connection.settimeout(22)
        return connection, address


class UnixConnection(http.client.HTTPConnection):
    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(SOCKET)


def serve():
    Path(SOCKET).unlink(missing_ok=True)
    with Server(SOCKET, Worker) as server:
        os.chmod(SOCKET, 0o660)
        server.serve_forever()


def main():
    if sys.argv[1:2] == ["--guest"]:
        return guest(sys.argv[2])
    if sys.argv[1:2] == ["--guest-self-test"]:
        return guest(sys.argv[2], diagnostics=True)
    if sys.argv[1:] == ["--health"]:
        client = UnixConnection("localhost", timeout=3)
        client.request("GET", "/health")
        return int(client.getresponse().status != 200)
    if sys.argv[1:] == ["--self-test"]:
        result = run_job({"code": "import csv\nprint(open('input.txt').read())\nopen('out.txt', 'w').write('done')",
                          "files": {"input.txt": "TinyBot"}}, diagnostics=True)
        if not result["ok"] or result["stdout"] != "TinyBot\n" or result["files"].get("out.txt") != "done":
            print(result, file=sys.stderr)
            return 1
        return 0
    serve()
    return 0


if __name__ == "__main__":
    sys.exit(main())
