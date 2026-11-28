"""Render bounded, structured reports as native Office documents."""
from io import BytesIO
import textwrap


MAX_OUTPUT_BYTES = 5 * 1024 * 1024
MAX_SLIDES = 120
NAVY = "102A43"
BLUE = "205493"
MUTED = "486581"
PALE = "EAF2F8"
WHITE = "FFFFFF"


def _cell_text(value):
    return str(value).lower() if isinstance(value, bool) else str(value)


def _docx(title, blocks, output):
    from docx import Document
    from docx.oxml import OxmlElement
    from docx.shared import Inches, Mm, Pt, RGBColor

    document = Document()
    document.core_properties.title = title
    document.core_properties.author = "TinyBot"
    section = document.sections[0]
    section.page_width = Mm(210)
    section.page_height = Mm(297)
    section.left_margin = section.right_margin = Inches(0.8)
    section.top_margin = section.bottom_margin = Inches(0.75)
    normal = document.styles["Normal"]
    normal.font.name = "Calibri"
    normal.font.size = Pt(11)
    normal.font.color.rgb = RGBColor.from_string(NAVY)
    normal.paragraph_format.space_after = Pt(8)
    document.styles["Title"].font.color.rgb = RGBColor.from_string(NAVY)
    for level in range(1, 4):
        document.styles[f"Heading {level}"].font.color.rgb = RGBColor.from_string(BLUE)

    document.add_heading(title, 0)
    for block in blocks:
        kind = block["type"]
        if kind == "heading":
            document.add_heading(block["text"], block.get("level", 1))
        elif kind == "paragraph":
            document.add_paragraph(block["text"])
        elif kind == "list":
            for item in block["items"]:
                document.add_paragraph(item, "List Bullet")
        elif kind == "table":
            rows = block["rows"]
            table = document.add_table(rows=len(rows), cols=len(rows[0]))
            table.style = "Light Shading Accent 1"
            table.rows[0]._tr.get_or_add_trPr().append(OxmlElement("w:tblHeader"))
            for index, values in enumerate(rows):
                row = table.rows[index]
                row._tr.get_or_add_trPr().append(OxmlElement("w:cantSplit"))
                for cell, value in zip(row.cells, values):
                    cell.text = _cell_text(value)
                    paragraph = cell.paragraphs[0]
                    paragraph.paragraph_format.space_after = Pt(4)
                    paragraph.runs[0].bold = index == 0
            document.add_paragraph()
    document.save(output)


def _text_lines(text, width):
    return [
        line
        for paragraph in text.splitlines() or [""]
        for line in textwrap.wrap(paragraph, width=width) or [""]
    ]


def _pptx(title, blocks, output):
    from pptx import Presentation
    from pptx.dml.color import RGBColor
    from pptx.enum.text import MSO_AUTO_SIZE
    from pptx.util import Inches, Pt

    deck = Presentation()
    deck.core_properties.title = title
    deck.core_properties.author = "TinyBot"
    deck.slide_width = Inches(13.333)
    deck.slide_height = Inches(7.5)

    def slide_with_heading(heading):
        if len(deck.slides) >= MAX_SLIDES:
            raise ValueError("This presentation exceeds the slide limit.")
        slide = deck.slides.add_slide(deck.slide_layouts[6])
        frame = slide.shapes.add_textbox(
            Inches(0.7), Inches(0.42), Inches(11.93), Inches(1.06),
        ).text_frame
        frame.word_wrap = True
        frame.auto_size = MSO_AUTO_SIZE.TEXT_TO_FIT_SHAPE
        paragraph = frame.paragraphs[0]
        paragraph.text = heading
        paragraph.font.name = "Calibri"
        paragraph.font.size = Pt(33)
        paragraph.font.bold = True
        paragraph.font.color.rgb = RGBColor.from_string(NAVY)
        return slide

    heading = title
    lines = []

    def flush_text():
        for offset in range(0, len(lines), 14):
            slide = slide_with_heading(heading)
            frame = slide.shapes.add_textbox(
                Inches(0.72), Inches(1.58), Inches(11.88), Inches(5.28),
            ).text_frame
            frame.word_wrap = True
            frame.auto_size = MSO_AUTO_SIZE.TEXT_TO_FIT_SHAPE
            for index, line in enumerate(lines[offset:offset + 14]):
                paragraph = frame.add_paragraph() if index else frame.paragraphs[0]
                paragraph.text = line
                paragraph.font.name = "Calibri"
                paragraph.font.size = Pt(21)
                paragraph.font.color.rgb = RGBColor.from_string(MUTED)
                paragraph.space_after = Pt(5)
        lines.clear()

    def add_table(rows):
        columns = len(rows[0])
        width = max(10, 118 // columns)
        heads = [_text_lines(_cell_text(value), width) for value in rows[0]]
        heading_lines = max(map(len, heads))
        if heading_lines > 5:
            raise ValueError("Use shorter headers in the slide table.")
        cell_height = 0.23
        table_height = 5.25
        header_height = 0.14 + cell_height * heading_lines
        max_data_lines = int((table_height - header_height - 0.14) / cell_height)
        page = [(heads, header_height)]
        used = header_height

        def flush_table():
            slide = slide_with_heading(heading)
            table = slide.shapes.add_table(
                len(page), columns, Inches(0.72), Inches(1.55), Inches(11.88), Inches(used),
            ).table
            for index, (values, height) in enumerate(page):
                table.rows[index].height = Inches(height)
                for column, wrapped in enumerate(values):
                    cell = table.cell(index, column)
                    cell.text = "\n".join(wrapped)
                    cell.margin_top = cell.margin_bottom = Inches(0.035)
                    cell.margin_left = cell.margin_right = Inches(0.09)
                    cell.text_frame.word_wrap = True
                    cell.fill.solid()
                    background = BLUE if index == 0 else PALE if index % 2 else WHITE
                    cell.fill.fore_color.rgb = RGBColor.from_string(background)
                    for paragraph in cell.text_frame.paragraphs:
                        paragraph.font.name = "Calibri"
                        paragraph.font.size = Pt(13)
                        paragraph.font.bold = index == 0
                        paragraph.font.color.rgb = RGBColor.from_string(WHITE if index == 0 else NAVY)

        for values in rows[1:]:
            wrapped = [_text_lines(_cell_text(value), width) for value in values]
            for offset in range(0, max(map(len, wrapped)), max_data_lines):
                part = [cell[offset:offset + max_data_lines] for cell in wrapped]
                height = 0.14 + cell_height * max(map(len, part))
                if used + height > table_height:
                    flush_table()
                    page = [(heads, header_height)]
                    used = header_height
                page.append((part, height))
                used += height
        flush_table()

    previous_was_heading = False
    for block in blocks:
        kind = block["type"]
        if kind == "heading":
            if previous_was_heading:
                slide_with_heading(heading)
            flush_text()
            heading = block["text"]
        elif kind == "paragraph":
            lines.extend(_text_lines(block["text"], 72))
            lines.append("")
        elif kind == "list":
            for item in block["items"]:
                first, *rest = _text_lines(item, 69)
                lines.extend(["• " + first, *("  " + line for line in rest)])
            lines.append("")
        elif kind == "table":
            flush_text()
            add_table(block["rows"])
        previous_was_heading = kind == "heading"
    flush_text()
    if previous_was_heading or not deck.slides:
        slide_with_heading(heading)
    deck.save(output)


def _xlsx(title, blocks, output):
    from openpyxl import Workbook
    from openpyxl.styles import Alignment, Font, PatternFill
    from openpyxl.utils import get_column_letter

    workbook = Workbook()
    workbook.properties.title = title
    workbook.properties.creator = "TinyBot"
    sheet = workbook.active
    sheet.title = "Report"
    sheet.freeze_panes = "A2"
    sheet.sheet_view.showGridLines = False
    columns = max(
        (len(block["rows"][0]) for block in blocks if block["type"] == "table"), default=1,
    )
    for column in range(1, columns + 1):
        sheet.column_dimensions[get_column_letter(column)].width = 52 if column == 1 else 22

    def add_row(values, *, heading=False, span_report=False):
        sheet.append(values)
        if span_report and columns > 1:
            sheet.merge_cells(start_row=sheet.max_row, end_row=sheet.max_row,
                              start_column=1, end_column=columns)
        for cell in sheet[sheet.max_row][:len(values)]:
            if isinstance(cell.value, str):
                cell.data_type = "s"
            cell.alignment = Alignment(vertical="top", wrap_text=True)
            cell.font = Font(name="Calibri", color=WHITE if heading else NAVY, bold=heading)
            if heading:
                cell.fill = PatternFill("solid", fgColor=BLUE)
        first_column_width = 50 + (22 * (columns - 1) if span_report else 0)
        lines = max(
            len(_text_lines(_cell_text(value), first_column_width if index == 0 else 20))
            for index, value in enumerate(values)
        )
        sheet.row_dimensions[sheet.max_row].height = min(405, max(20, 16 * lines))

    add_row([title], heading=True, span_report=True)
    for block in blocks:
        kind = block["type"]
        if kind in ("heading", "paragraph"):
            add_row([block["text"]], heading=kind == "heading", span_report=True)
        elif kind == "list":
            for item in block["items"]:
                add_row(["• " + item], span_report=True)
        elif kind == "table":
            for index, values in enumerate(block["rows"]):
                add_row(values, heading=index == 0)
    workbook.save(output)


OFFICE_RENDERERS = {"docx": _docx, "pptx": _pptx, "xlsx": _xlsx}


def render_office(format, title, blocks):
    output = BytesIO()
    render = OFFICE_RENDERERS.get(format)
    if render is None:
        raise ValueError("Unsupported Office format.")
    render(title, blocks, output)
    if output.tell() > MAX_OUTPUT_BYTES:
        raise ValueError("The generated document exceeds its size limit.")
    return output.getvalue()
