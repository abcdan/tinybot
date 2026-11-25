#!/usr/bin/env python3
"""Verify a labelled TinyBot stack against the assets in this checkout."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import sys
from urllib.parse import urlsplit
from urllib.request import HTTPRedirectHandler, build_opener


class NoRedirects(HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, msg, headers, newurl):
        raise ValueError(f"Unexpected redirect for {urlsplit(request.full_url).path}")


def verify(origin, expected, web):
    opener = build_opener(NoRedirects())

    def download(path):
        url = origin + path
        with opener.open(url, timeout=10) as response:
            body = response.read(1_000_001)
        if len(body) > 1_000_000:
            raise ValueError(f"Oversized response for {path}")
        return body

    ready = json.loads(download("/ready"))
    if not isinstance(ready, dict) or ready.get("ready") is not True or ready.get("release") != expected:
        raise ValueError("Core readiness or release revision does not match")
    for name in ("browser", "sandbox"):
        status = ready.get(name) or {}
        if not isinstance(status, dict) or status.get("ok") is not True or status.get("release") != expected:
            raise ValueError(f"{name} readiness or release revision does not match")
    routes = {"/mini": web / "mini.html", "/app.css": web / "app.css"}
    routes.update({"/" + path.name: path for path in sorted(web.glob("mini*.js"))})
    for route, path in routes.items():
        if hashlib.sha256(download(route)).digest() != hashlib.sha256(path.read_bytes()).digest():
            raise ValueError(f"Mini App asset differs from the checkout: {route}")
    return len(routes)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("origin")
    parser.add_argument("release_sha")
    args = parser.parse_args()
    parts = urlsplit(args.origin)
    if (parts.scheme != "https" and not (parts.scheme == "http" and parts.hostname in {"127.0.0.1", "localhost", "::1"})) or not parts.hostname or parts.username or parts.password or parts.path not in {"", "/"} or parts.query or parts.fragment:
        parser.error("Use an HTTPS origin, or an HTTP origin on loopback for a local fixture")
    if not re.fullmatch(r"[0-9a-f]{40}", args.release_sha):
        parser.error("Use a full 40-character Git commit SHA")
    try:
        count = verify(args.origin.rstrip("/"), args.release_sha, Path(__file__).resolve().parents[1] / "web")
    except (OSError, ValueError) as error:
        print(f"TinyBot release verification failed: {error}", file=sys.stderr)
        return 1
    print(f"Verified {args.release_sha}, both workers, and {count} Mini App assets.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
