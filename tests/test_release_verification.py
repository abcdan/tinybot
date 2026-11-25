import importlib.util
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import tempfile
from threading import Thread
import unittest


spec = importlib.util.spec_from_file_location(
    "verify_release", Path(__file__).resolve().parents[1] / "deploy" / "verify-release.py"
)
verifier = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verifier)


class ReleaseVerificationTest(unittest.TestCase):
    def test_verified_stack_rejects_a_stale_asset_and_worker(self):
        revision = "a" * 40
        worker = {"ok": True, "release": revision}
        identity = {"ready": True, "release": revision, "browser": worker, "sandbox": worker}
        assets = {"/mini": b"<title>Fixture</title>", "/app.css": b"body {}", "/mini.js": b"'fixture';"}

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                body = json.dumps(identity).encode() if self.path == "/ready" else assets[self.path]
                self.send_response(200)
                self.end_headers()
                self.wfile.write(body)

        with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server, tempfile.TemporaryDirectory() as out:
            thread = Thread(target=server.serve_forever)
            thread.start()
            try:
                web = Path(out)
                for route, content in assets.items():
                    (web / ("mini.html" if route == "/mini" else route[1:])).write_bytes(content)
                origin = f"http://127.0.0.1:{server.server_port}"
                self.assertEqual(verifier.verify(origin, revision, web), 3)
                assets["/mini.js"] = b"'old';"
                with self.assertRaisesRegex(ValueError, "Mini App asset differs"):
                    verifier.verify(origin, revision, web)
                identity["sandbox"] = {"ok": True, "release": "b" * 40}
                with self.assertRaisesRegex(ValueError, "sandbox readiness or release revision"):
                    verifier.verify(origin, revision, web)
            finally:
                server.shutdown()
                thread.join()


if __name__ == "__main__":
    unittest.main()
