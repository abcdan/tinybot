"""Validate and render native documents from structured text and numbers."""

import json
import math

REQUEST_LIMIT = 96 * 1024
CONTENT_LIMIT = 64 * 1024
OUTPUT_LIMIT = 8 * 1024 * 1024
MIME_TYPES = {
    "pdf": "application/pdf",
    "docx": "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "pptx": "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    "xlsx": "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
}


class InvalidDocument(ValueError):
    pass


def unique_fields(pairs):
    fields = dict(pairs)
    if len(fields) != len(pairs):
        raise InvalidDocument("Document fields must have unique names.")
    return fields


def validate_fields(value, required, optional=frozenset()):
    if not isinstance(value, dict) or required - value.keys() or value.keys() - required - optional:
        raise InvalidDocument("Use only the supported fields for this document.")


def text_size(value, limit, *, allow_empty=False):
    if not isinstance(value, str) or (not allow_empty and not value.strip()):
        raise InvalidDocument("Supply text for each document section.")
    size = len(value.encode("utf-8"))
    if size > limit:
        raise InvalidDocument("A document field exceeds its text limit.")
    if any(ord(char) < 32 and char not in "\t\r\n" or char in "\ufffe\uffff" for char in value):
        raise InvalidDocument("Document text contains unsupported control characters.")
    return size


def cell_size(value):
    if isinstance(value, str):
        return text_size(value, 1000, allow_empty=True)
    if isinstance(value, bool):
        return 4 if value else 5
    if isinstance(value, int) and -(2 ** 63) <= value < 2 ** 64:
        return len(str(value))
    if isinstance(value, float) and math.isfinite(value):
        return len(str(value))
    raise InvalidDocument("Table cells must be text, booleans or finite numbers.")


def block_size(block):
    if not isinstance(block, dict):
        raise InvalidDocument("Use structured document blocks.")
    match block.get("type"):
        case "heading":
            validate_fields(block, {"type", "text"}, {"level"})
            level = block.get("level", 2)
            if type(level) is not int or not 1 <= level <= 3:
                raise InvalidDocument("Use heading levels from 1 to 3.")
            return text_size(block["text"], 200)
        case "paragraph":
            validate_fields(block, {"type", "text"})
            return text_size(block["text"], 4000)
        case "list":
            validate_fields(block, {"type", "items"})
            items = block["items"]
            if not isinstance(items, list) or not 1 <= len(items) <= 40:
                raise InvalidDocument("Use 1 to 40 items in a document list.")
            return sum(text_size(item, 1000) for item in items)
        case "table":
            validate_fields(block, {"type", "rows"})
            rows = block["rows"]
            if not isinstance(rows, list) or not 1 <= len(rows) <= 100:
                raise InvalidDocument("Use 1 to 100 rows in a document table.")
            if not isinstance(rows[0], list) or not 1 <= (columns := len(rows[0])) <= 8:
                raise InvalidDocument("Use 1 to 8 columns in a document table.")
            if any(not isinstance(row, list) or len(row) != columns for row in rows):
                raise InvalidDocument("All rows must have the same number of columns.")
            return sum(cell_size(cell) for row in rows for cell in row)
        case _:
            raise InvalidDocument("Use a heading, paragraph, list or table block.")


def render_document(body):
    data = json.loads(body.decode("utf-8"), object_pairs_hook=unique_fields)
    validate_fields(data, {"format", "title", "blocks"})
    format = data["format"]
    if not isinstance(format, str) or format not in MIME_TYPES:
        raise InvalidDocument("Choose PDF, Word, PowerPoint or Excel.")
    title, blocks = data["title"], data["blocks"]
    size = text_size(title, 200)
    if not isinstance(blocks, list) or not 1 <= len(blocks) <= 40:
        raise InvalidDocument("Use 1 to 40 document blocks.")
    for block in blocks:
        size += block_size(block)
        if size > CONTENT_LIMIT:
            raise InvalidDocument("The document exceeds its content size limit.")

    try:
        if format == "pdf":
            from pdf_document import render_pdf
            output = render_pdf(title, blocks)
        else:
            from office import render_office
            output = render_office(format, title, blocks)
    except ValueError:
        raise InvalidDocument("The content does not fit in the requested document format.") from None
    if not output or len(output) > OUTPUT_LIMIT:
        raise InvalidDocument("The generated document exceeds its file size limit.")
    return MIME_TYPES[format], output
