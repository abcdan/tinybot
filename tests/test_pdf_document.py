"""Exercise the PDF renderer with a PDF parser and installed DejaVu Sans fonts."""

from io import BytesIO
import os
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

from pypdf import PdfReader

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "workers" / "sandbox"))
from pdf_document import FONT_DIRECTORY, render_pdf


FONT_PATH = Path(os.environ.get("TINYBOT_TEST_FONT_DIR", FONT_DIRECTORY))
TITLE = "Eiwitrijke maaltijden – prijsvergelijking"
COMPARISON = {
    "type": "table",
    "rows": [
        ["Maaltijd", "Portie", "Prijs", "Eiwit"],
        ["Kip tikka masala", "450 g", "€4,46", 42.3],
        ["Maaltijdpakket", "12 maaltijden", "€5,19", 30],
    ],
}
BLOCKS = [
    {"type": "heading", "text": "Prijsvergelijking", "level": 1},
    COMPARISON,
    {"type": "heading", "text": "Praktische aandachtspunten"},
    {"type": "paragraph", "text": "Een maaltijd met 42,3 g eiwit. Café in Zürich."},
    {"type": "list", "items": ["Reken de bezorgkosten mee.", "Controleer actuele prijzen."]},
]


class PdfDocumentTest(unittest.TestCase):
    def render(self, blocks):
        return render_pdf(TITLE, blocks, font_directory=FONT_PATH)

    def test_generates_real_pdf_with_unicode_and_literal_content(self):
        text = '<img src="https://fixture.invalid/image"> & <b>letterlijke tekst</b>'
        blocks = [*BLOCKS, {"type": "paragraph", "text": text}]
        document = self.render(blocks)
        self.assertTrue(document.startswith(b"%PDF-"))
        self.assertEqual(document, self.render(blocks))
        reader = PdfReader(BytesIO(document))
        self.assertEqual(reader.metadata.title, TITLE)
        self.assertEqual(len(reader.pages), 1)
        self.assertAlmostEqual(float(reader.pages[0].mediabox.width), 595.28, places=2)
        contents = " ".join(reader.pages[0].extract_text().split())
        for phrase in [TITLE, "€4,46", "Zürich", "42,3 g", "Controleer actuele prijzen.", text]:
            self.assertIn(phrase, contents)
        self.assertNotIn("/Annots", reader.pages[0])

    def test_long_table_repeats_headers_across_pages_without_losing_rows(self):
        rows = [["Productcode", "Omschrijving", "Prijs"]]
        for index in range(1, 89):
            rows.append([f"ID-{index:03}", "Ruimte voor een omschrijving die op meerdere regels kan staan.", index])
        data = self.render([{"type": "table", "rows": rows}])
        pages = PdfReader(BytesIO(data)).pages
        self.assertGreater(len(pages), 1)
        contents = [page.extract_text() for page in pages]
        for text in contents:
            self.assertEqual(text.count("Productcode"), 1)
            self.assertEqual(text.count("Omschrijving"), 1)
        for index in range(1, 89):
            self.assertEqual(sum(text.count(f"ID-{index:03}") for text in contents), 1)
        with patch("pdf_document.MAX_PAGES", 1):
            with self.assertRaisesRegex(ValueError, "page limit"):
                self.render([{"type": "table", "rows": rows}])


if __name__ == "__main__":
    unittest.main()
