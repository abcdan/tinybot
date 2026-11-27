"""Render validated document blocks as a printable PDF."""

from functools import lru_cache
from io import BytesIO
from pathlib import Path
from xml.sax.saxutils import escape

from reportlab.lib import colors
from reportlab.lib.enums import TA_RIGHT
from reportlab.lib.pagesizes import A4
from reportlab.lib.styles import ParagraphStyle
from reportlab.pdfbase import pdfmetrics
from reportlab.pdfbase.ttfonts import TTFont
from reportlab.platypus import LongTable, PageBreak, Paragraph, SimpleDocTemplate, TableStyle
from reportlab.platypus.doctemplate import LayoutError

FONT_DIRECTORY = Path("/usr/share/fonts/truetype/dejavu")
REGULAR = "TinyBotSans"
BOLD = "TinyBotSans-Bold"
MARGIN = 42
MAX_PAGES = 64
MAX_PDF_BYTES = 4 * 1024 * 1024

INK = colors.HexColor("#172638")
SECONDARY = colors.HexColor("#516174")
LINE = colors.HexColor("#D9E2EC")
STRIPE = colors.HexColor("#F2F6FA")


@lru_cache(maxsize=1)
def register_fonts(directory):
    pdfmetrics.registerFont(TTFont(REGULAR, str(directory / "DejaVuSans.ttf")))
    pdfmetrics.registerFont(TTFont(BOLD, str(directory / "DejaVuSans-Bold.ttf")))


def paragraph(text, style, bullet=None):
    # Paragraph accepts markup, so only the renderer may insert markup.
    markup = escape(text.replace("\r\n", "\n").replace("\r", "\n")).replace("\n", "<br/>")
    return Paragraph(markup, style, bulletText=bullet)


def styles():
    body = ParagraphStyle("Body", fontName=REGULAR, fontSize=10, leading=15,
                          textColor=INK, spaceAfter=8, splitLongWords=True)
    title = ParagraphStyle("Title", parent=body, fontName=BOLD, fontSize=26, leading=32,
                           spaceAfter=18, keepWithNext=True)
    headings = [
        ParagraphStyle(f"Heading{level}", parent=body, fontName=BOLD, fontSize=size,
                       leading=size + 5, spaceBefore=10, spaceAfter=5, keepWithNext=True)
        for level, size in enumerate((16, 13, 11), start=1)
    ]
    bullet = ParagraphStyle("Bullet", parent=body, leftIndent=14, bulletIndent=2,
                            bulletFontName=REGULAR, bulletFontSize=9, spaceAfter=4)
    cell = ParagraphStyle("Cell", parent=body, fontSize=9, leading=13, spaceAfter=0)
    header = ParagraphStyle("ColumnHeader", parent=cell, fontName=BOLD, textColor=colors.white)
    number = ParagraphStyle("NumericCell", parent=cell, alignment=TA_RIGHT)
    return title, headings, body, bullet, cell, header, number


class PaginatedTable(LongTable):
    def split(self, width, height):
        fragments = super().split(width, height)
        if len(fragments) == 2:
            # A repeated header belongs at the start of a page.
            return [fragments[0], PageBreak(), fragments[1]]
        return fragments


def table(rows, width, cell, header, number):
    values = []
    for index, row in enumerate(rows):
        values.append([
            paragraph(
                str(value),
                header if index == 0 else number if type(value) in (int, float) else cell,
            )
            for value in row
        ])
    result = PaginatedTable(
        values, colWidths=[width / len(rows[0])] * len(rows[0]), repeatRows=1,
        splitInRow=1, hAlign="LEFT", spaceBefore=3, spaceAfter=12,
    )
    result.setStyle(TableStyle([
        ("BACKGROUND", (0, 0), (-1, 0), INK),
        ("ROWBACKGROUNDS", (0, 1), (-1, -1), (colors.white, STRIPE)),
        ("LINEBELOW", (0, 0), (-1, -1), 0.45, LINE),
        ("VALIGN", (0, 0), (-1, -1), "TOP"),
        ("LEFTPADDING", (0, 0), (-1, -1), 7),
        ("RIGHTPADDING", (0, 0), (-1, -1), 7),
        ("TOPPADDING", (0, 0), (-1, -1), 6),
        ("BOTTOMPADDING", (0, 0), (-1, -1), 6),
    ]))
    return result


def footer(canvas, document):
    if document.page > MAX_PAGES:
        raise ValueError("The PDF exceeds the page limit.")
    canvas.saveState()
    canvas.setStrokeColor(LINE)
    canvas.line(MARGIN, 34, A4[0] - MARGIN, 34)
    canvas.setFillColor(SECONDARY)
    canvas.setFont(REGULAR, 8)
    canvas.drawRightString(A4[0] - MARGIN, 22, str(document.page))
    canvas.restoreState()


def render_pdf(title, blocks, *, font_directory=FONT_DIRECTORY):
    register_fonts(font_directory)
    output = BytesIO()
    document = SimpleDocTemplate(
        output, pagesize=A4, leftMargin=MARGIN, rightMargin=MARGIN,
        topMargin=50, bottomMargin=53, title=title, author="TinyBot",
        pageCompression=1, invariant=1,
    )
    title_style, headings, body, bullet, cell, header, number = styles()
    story = [paragraph(title, title_style)]
    for block in blocks:
        kind = block["type"]
        if kind == "heading":
            story.append(paragraph(block["text"], headings[block.get("level", 2) - 1]))
        elif kind == "paragraph":
            story.append(paragraph(block["text"], body))
        elif kind == "list":
            story.extend(paragraph(item, bullet, "•") for item in block["items"])
        elif kind == "table":
            story.append(table(block["rows"], document.width - 12, cell, header, number))

    try:
        document.build(story, onFirstPage=footer, onLaterPages=footer)
    except LayoutError:
        raise ValueError("The PDF contains a heading or table that will not fit on a page.") from None
    if output.tell() > MAX_PDF_BYTES:
        raise ValueError("The PDF exceeds the file size limit.")
    return output.getvalue()
