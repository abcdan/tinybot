"""Run inside the sandbox service to exercise its actual Unix socket."""
import json
import sys

sys.path.insert(0, "/opt/tinybot")
from server import UnixConnection


def run(code, files=None):
    client = UnixConnection("localhost", timeout=35)
    client.request("POST", "/run", json.dumps({"code": code, "files": files or {}}),
                   {"Content-Type": "application/json"})
    response = client.getresponse()
    body = json.load(response)
    client.close()
    return response.status, body


status, result = run("import csv\nprint(open('input.txt').read())\nopen('output.txt','w').write('saved')",
                     {"input.txt": "seed"})
assert status == 200 and result["ok"] and result["stdout"] == "seed\n", result
assert result["files"] == {"input.txt": "seed", "output.txt": "saved"}, result
status, result = run("from pathlib import Path\nprint(Path('input.txt').exists(), Path('/etc/passwd').exists(), Path('/data/tinybot.sqlite').exists())")
assert status == 200 and result["stdout"] == "False False False\n", result
status, result = run("while True: pass")
assert status == 200 and not result["ok"] and not result["files"], result
status, result = run("with open('huge.txt','w') as file:\n    file.write('x' * 70000)")
assert status == 200 and not result["ok"] and not result["files"], result.get("stderr")
status, result = run("bytearray(150 * 1024 * 1024)")
assert status == 200 and not result["ok"] and not result["files"], result.get("stderr")
status, result = run("print('hello')", {"../outside.txt": "no"})
assert status == 422, result
print("Python worker passed file mutation, cross-job isolation, host path isolation, execution limits, and path validation.")
