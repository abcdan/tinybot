"""Reopen generated native Office documents and check their editable content."""
from io import BytesIO
from pathlib import Path
import sys
from xml.etree import ElementTree
from zipfile import ZipFile

from docx import Document
from openpyxl import load_workbook
from pptx import Presentation

sys.path.insert(0, "/opt/tinybot")
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "workers" / "sandbox"))
from office import render_office


TITLE = "Meal comparison"
BLOCKS = [
    {"type": "heading", "text": "Results", "level": 1},
    {"type": "paragraph", "text": "Compare price & portions."},
    {"type": "list", "items": ["Review delivery fees", "Check availability"]},
    {"type": "table", "rows": [
        ["Meal", "Price", "In stock"],
        ["Chicken", 4.46, True],
        ["=1+1", "€5,19", False],
    ]},
]


def checked_bytes(format, blocks=BLOCKS):
    data = render_office(format, TITLE, blocks)
    with ZipFile(BytesIO(data)) as package:
        assert package.testzip() is None
        for name in package.namelist():
            assert "vbaProject" not in name
            if name.endswith(".rels"):
                relationships = ElementTree.fromstring(package.read(name))
                assert all(node.get("TargetMode") != "External" for node in relationships)
    return BytesIO(data)


def main():
    word = Document(checked_bytes("docx"))
    assert word.paragraphs[0].text == TITLE
    assert word.paragraphs[1].style.name == "Heading 1"
    assert word.paragraphs[2].text == "Compare price & portions."
    assert word.paragraphs[3].style.name == "List Bullet"
    assert word.tables[0].cell(2, 1).text == "€5,19"

    rows = [["Meal", "Price"]] + [[f"Meal {number}", number] for number in range(100)]
    presentation = Presentation(checked_bytes("pptx", BLOCKS + [{"type": "table", "rows": rows}]))
    assert presentation.core_properties.title == TITLE
    assert any(shape.has_text_frame and "delivery fees" in shape.text
               for slide in presentation.slides for shape in slide.shapes)
    tables = [shape.table for slide in presentation.slides for shape in slide.shapes if shape.has_table]
    assert len(tables) > 2
    assert all(table.cell(0, 0).text == "Meal" for table in tables)
    assert sum(len(table.rows) - 1 for table in tables) == 102
    assert tables[-1].cell(len(tables[-1].rows) - 1, 0).text == "Meal 99"

    workbook = load_workbook(checked_bytes("xlsx"))
    sheet = workbook.active
    assert sheet["A1"].value == TITLE
    assert {str(cells) for cells in sheet.merged_cells.ranges} == {
        "A1:C1", "A2:C2", "A3:C3", "A4:C4", "A5:C5",
    }
    assert sheet["B7"].value == 4.46 and sheet["B7"].data_type == "n"
    assert sheet["C7"].value is True
    assert sheet["A8"].value == "=1+1" and sheet["A8"].data_type == "s"
    assert sheet["C8"].value is False
    workbook.close()
    print("DOCX, PPTX, and XLSX render and reopen with native editable content.")


if __name__ == "__main__":
    main()
