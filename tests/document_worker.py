"""Exercise native document rendering over the sandbox's actual Unix socket."""

from io import BytesIO
import json
import sys
import time
from zipfile import ZipFile

sys.path.insert(0, "/opt/tinybot")
from server import UnixConnection


FORMATS = {
    "pdf": ("application/pdf", None),
    "docx": ("application/vnd.openxmlformats-officedocument.wordprocessingml.document", "word/document.xml"),
    "pptx": ("application/vnd.openxmlformats-officedocument.presentationml.presentation", "ppt/presentation.xml"),
    "xlsx": ("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", "xl/workbook.xml"),
}


def render(data, *, declared_length=None):
    body = data if isinstance(data, bytes) else json.dumps(data).encode("utf-8")
    headers = {"Content-Type": "application/json"}
    if declared_length is not None:
        headers["Content-Length"] = str(declared_length)
    client = UnixConnection("localhost", timeout=20)
    try:
        client.request("POST", "/render-document", body, headers)
        response = client.getresponse()
        return response.status, response.headers, response.read()
    finally:
        client.close()


def wait_for_worker():
    for _ in range(20):
        client = UnixConnection("localhost", timeout=2)
        try:
            client.request("GET", "/health")
            if client.getresponse().status == 200:
                return
        except OSError:
            time.sleep(0.1)
        finally:
            client.close()
    raise RuntimeError("sandbox health endpoint did not start")


def main():
    wait_for_worker()
    fixture = {
        "format": "pdf",
        "title": "Prijsvergelijking in Zürich",
        "blocks": [
            {"type": "heading", "level": 2, "text": "Maaltijden"},
            {"type": "paragraph", "text": "Een vergelijking van prijzen en porties."},
            {"type": "list", "items": ["Controleer bezorgkosten."]},
            {"type": "table", "rows": [
                ["Maaltijd", "Prijs", "Beschikbaar"],
                ["Kip tikka masala", 4.46, True],
                ["Maaltijdpakket", "€5,19", False],
            ]},
        ],
    }
    for format, (mime, part) in FORMATS.items():
        status, headers, data = render({**fixture, "format": format})
        assert status == 200 and headers["Content-Type"] == mime, (format, status)
        assert headers["Cache-Control"] == "no-store" and int(headers["Content-Length"]) == len(data)
        if part:
            with ZipFile(BytesIO(data)) as package:
                assert part in package.namelist() and package.testzip() is None, format
        else:
            assert data.startswith(b"%PDF-") and data.rstrip().endswith(b"%%EOF")

    invalid = [
        {**fixture, "format": "md"},
        {**fixture, "filename": "../../private-document"},
        {**fixture, "title": "€" * 67},
        {**fixture, "title": "private\x00document"},
        {**fixture, "blocks": [{"type": "heading", "text": "Relative levels", "level": None}]},
        {**fixture, "blocks": [{"type": "list", "items": ["Note"] * 41}]},
        {**fixture, "blocks": [{"type": "table", "rows": [["Column"], ["Wrong", "Width"]]}]},
        {**fixture, "blocks": [{"type": "table", "rows": [[float("inf")]]}]},
        {**fixture, "blocks": [{"type": "table", "rows": [["Column"]] * 101}]},
        {**fixture, "blocks": [{"type": "paragraph", "text": "x" * 4000}] * 17},
        b'{"format":"pdf","format":"xlsx","title":"private-document","blocks":[]}',
    ]
    for request in invalid:
        status, headers, data = render(request)
        assert status == 422 and headers["Content-Type"] == "application/json", status
        assert set(json.loads(data)) == {"error"} and b"private" not in data
    status, headers, data = render(b"", declared_length=96 * 1024 + 1)
    assert status == 422 and headers["Content-Type"] == "application/json", status
    assert "size limit" in json.loads(data)["error"]
    print("Sandbox rendered all four native formats and rejected malformed or oversized documents.")


if __name__ == "__main__":
    main()
