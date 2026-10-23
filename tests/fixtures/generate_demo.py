"""Generate Finch's deterministic, original 16-page reading fixture (ReportLab)."""
from pathlib import Path
from reportlab.pdfgen import canvas
from reportlab.platypus import Paragraph
from reportlab.lib.styles import ParagraphStyle
from reportlab.lib.colors import HexColor, white
from reportlab.lib.enums import TA_JUSTIFY

OUTPUT = Path(__file__).with_name("finch-demo.pdf")
NAVY = HexColor("#142E42")
TEAL = HexColor("#087E8B")
INK = HexColor("#283743")
MUTED = HexColor("#667783")
PALE = HexColor("#EFF6F7")
BODY = ParagraphStyle("body", fontName="Times-Roman", fontSize=10.3, leading=14.3,
                      textColor=INK, alignment=TA_JUSTIFY, spaceAfter=10)
SMALL = ParagraphStyle("small", parent=BODY, fontSize=9.1, leading=12.7)
HEAD = ParagraphStyle("heading", fontName="Helvetica-Bold", fontSize=12, leading=16, textColor=NAVY)

SECTIONS = [
    ("A reader that remembers", "The working set is the reader's attention",
     "A paper reader has a small but demanding task: put the right page in front of a person, keep it readable, and remember the place where work stopped. Finch treats this moment as the center of the application. A local document can be useful before a catalog has loaded or a plugin has been discovered.",
     "Opening a document is a sequence of visible commitments. The window should respond first. The current page follows. Details that do not change the next reading action can arrive later. This ordering makes a slow document understandable without pretending that every operation is instantaneous."),
    ("Restoring a reading position", "Coordinates survive a changing window",
     "A saved page number is only a coarse location. A reader may stop halfway through an equation or between two columns. Store a point on the page and the place where that point appears in the viewport. These two facts describe the reading position even when the window is resized.",
     "A restoration record also carries the document identity, its file revision, and a zoom mode. Explicit scale and fit-to-width are different intentions. A change in display density should preserve the intended content size while leaving the page anchor stable."),
    ("The visible page comes first", "Rendering is a scheduling problem",
     "A document with a thousand pages does not require a thousand page images. The immediate workload is a handful of visible tiles. Requests near the viewport have priority; speculative work is useful only when it does not delay those tiles. Finch uses bounded queues to make this tradeoff explicit.",
     "Every render request names the document generation that created it. A later scroll, zoom, or file change can make that request obsolete. Cancellation saves work where the engine offers a pause point. Otherwise the completed result is discarded before it can replace newer content."),
    ("Budgets make caches predictable", "Count bytes rather than entries",
     "A cache with ten entries can be tiny or enormous. A byte budget expresses the resource that actually matters. Tile buffers, copied upload buffers, and retained textures each have a lifetime, so memory accounting follows ownership rather than the number of pages visited.",
     "Eviction should preserve the current view before preserving distant history. A useful experiment scrolls forward through a document, returns to a nearby page, and then jumps far away. This reveals both locality and whether old generations continue to occupy the cache."),
    ("Wide figures and mixed pages", "One document can contain several geometries",
     "This landscape page is intentionally wider than the preceding pages. Mixed page sizes expose assumptions that disappear in a uniform document. The layout should use the dimensions of each page while retaining a stable reading anchor as new dimensions become available.",
     "A wide figure can be inspected at fit-to-width or at a deliberate scale. Horizontal position matters in the second case. A user should not lose the relevant part of a diagram simply because a neighboring portrait page has a different width."),
    ("Text is more than an image", "Selection needs the same coordinate system",
     "A rendered tile tells the eye where letters are. Text extraction tells the application what those letters mean. Selection joins these representations using page-space bounds. The same conversion should support highlights, link hit testing, and navigation targets.",
     "A rotated page is a particularly useful check. A text box that appears correct on an unrotated page can drift after rotation if only one side of the transformation is adjusted. Test points at the corners as well as near the center of the page."),
    ("Search without blocking reading", "Results can arrive incrementally",
     "Finch appears throughout this original fixture so that document search has several meaningful matches. A search request is a visitor to the PDF worker, not its permanent owner. Visible rendering can take precedence between pages while partial matches become available.",
     "Changing the query invalidates earlier results. Keep the query generation with each response, just as rendering keeps the document generation. This avoids a familiar interface error: a result list that silently combines two different user requests."),
    ("Navigation is a local operation", "Bookmarks, links, and page numbers agree",
     "The bookmark tree in this fixture contains one entry for every page. The first page also links to the wide-page section. These are internal destinations and should move the reading position without reopening the file or discarding the current document identity.",
     "External URI links are a separate kind of destination. This fixture points to example.org, a deliberately illustrative address. Displaying a link and opening a link are separate actions; the latter follows an explicit reader gesture."),
    ("A smaller sheet", "Compact pages still need comfortable margins",
     "Finch displays this page on a smaller sheet. The text remains real selectable content, so zooming can be checked without confusing bitmap sharpness with page layout. The footer and heading stay inside the page bounds rather than relying on the dimensions of the first page.",
     "Page-size changes are also useful restoration tests. Stop near the bottom of this sheet, close the application, then reopen it with a different window width. The same page-space anchor should remain the reference point."),
    ("A library of references", "The original file remains the source",
     "A lightweight library records a reference to a local PDF. Importing the reference need not duplicate the file. Titles, tags, favorites, and recent reading information help a person return to a paper without converting the library into a second filesystem.",
     "Files do move. A missing reference should remain visible and offer a way to reconnect it. Removing a library record is distinct from deleting the original PDF. This distinction belongs in the application behavior, not merely in documentation."),
    ("Saving small pieces of state", "Durability should not interrupt attention",
     "Scrolling can change the reading position dozens of times per second. Writing every change is unnecessary. A short debounce coalesces nearby changes, while a maximum interval prevents continuous interaction from postponing persistence indefinitely.",
     "A normal exit flushes the final state. A forced exit is different: the last durable snapshot is the recovery point. Atomic replacement ensures that recovery sees either the prior complete snapshot or the new complete snapshot, rather than a partially written record."),
    ("Plugins at the edge", "A command earns its process",
     "A translation command or a note-taking command can extend Finch without changing the PDF engine. The host supplies a small context: document information, page index, and selected text. A plain-text result is sufficient for the first executable plugin protocol.",
     "Trusted native plugins run only after an explicit command. The process boundary helps contain ordinary crashes, but it is not a security sandbox. A protocol needs version negotiation, bounded messages, deadlines, and a clear cleanup path when a child stops responding."),
    ("A second portrait format", "Letter-sized paper joins the sequence",
     "This page uses US Letter dimensions. Together with the wide and compact pages it tests layout across several common geometries. A page index always identifies the same content, regardless of how many pixels are needed to display that content at the current scale.",
     "The diagram below is a conceptual pipeline rather than a measured performance chart. It shows why the data crossing module boundaries should be simple: document identifiers, page coordinates, requests, and results. Native engine handles stay inside their owning worker."),
    ("Measure what the reader sees", "A milestone is not a whole experience",
     "A window-open timestamp measures one stage. It does not prove that the recovered page is legible. Tile completion is another stage, and actual presentation is another. Report these separately rather than using a convenient internal event as a substitute for every user-visible outcome.",
     "Warm startup and cold startup answer different questions. Repeated runs naturally warm filesystem and driver caches. A cold sample needs an explicit environmental procedure. Do not label the first run of a script as a cold boot unless those conditions were controlled."),
    ("Memory and idle time", "Independent observations catch blind spots",
     "A tile-cache counter describes application-owned buffers. It does not include every allocation in a PDF parser, a renderer, or the operating system. Process private bytes and working set are useful independent observations, while GPU memory needs a separate measurement method.",
     "Idle CPU should be sampled during a quiet interval without automation that repeatedly moves the pointer, captures a screenshot, or changes the window. Record the duration and normalization convention. A small number without its method is difficult to compare."),
    ("A reproducible reading exercise", "The fixture is a tool, not a benchmark result",
     "This document is original authored content for Finch development. Its sixteen pages provide selectable text, headings, simple equations, vector diagrams, bookmarks, internal navigation, an external URI, and mixed page dimensions. It contains no third-party paper or remote media.",
     "The equations and diagrams are conceptual. They do not report measured results or make claims about completed performance targets. Use the fixture to make tests repeatable, and keep the actual acceptance evidence in a separate report with commands, dates, hardware, and limitations."),
]

def paragraph(pdf, value, x, top, width, style=BODY):
    item = Paragraph(value, style)
    _, height = item.wrap(width, 1000)
    assert top - height > 55, (value[:30], top, height)
    item.drawOn(pdf, x, top - height)
    return top - height - 11

def footer(pdf, width, page):
    pdf.setStrokeColor(HexColor("#D4E1E5")); pdf.setLineWidth(.5)
    pdf.line(44, 45, width - 44, 45)
    pdf.setFont("Helvetica", 8); pdf.setFillColor(MUTED)
    pdf.drawString(44, 29, "FINCH ENGINEERING NOTES  /  ORIGINAL TEST FIXTURE")
    pdf.drawRightString(width - 44, 29, f"{page:02d} / 16")

def diagram(pdf, x, y, width, labels):
    gap = 13; cell = (width - gap * 2) / 3
    for index, label in enumerate(labels):
        left = x + index * (cell + gap)
        pdf.setFillColor(PALE); pdf.setStrokeColor(HexColor("#AFCFD3"))
        pdf.roundRect(left, y, cell, 40, 5, fill=1, stroke=1)
        pdf.setFont("Helvetica-Bold", 9); pdf.setFillColor(TEAL)
        pdf.drawCentredString(left + cell / 2, y + 15, label)
        if index < 2:
            pdf.setStrokeColor(TEAL); pdf.line(left + cell + 3, y + 20, left + cell + gap - 3, y + 20)

pdf = canvas.Canvas(str(OUTPUT), pagesize=(595.276, 841.89), invariant=1, pageCompression=1)
pdf.setTitle("Finch: Local-first Paper Reading")
pdf.setAuthor("Finch contributors")
pdf.setSubject("Original deterministic reading, navigation, search, and restoration fixture")
pdf.setKeywords("Finch, local-first, PDF, reader, deterministic fixture")
for index, (title, subtitle, first, second) in enumerate(SECTIONS):
    page = index + 1
    width, height = (792, 612) if page == 5 else (420, 595) if page == 9 else (612, 792) if page == 13 else (595.276, 841.89)
    pdf.setPageSize((width, height))
    pdf.bookmarkPage(f"page-{page}")
    pdf.addOutlineEntry(f"{page:02d}  {title}", f"page-{page}", level=0)
    pdf.setFillColor(TEAL); pdf.rect(44, height - 47, 31, 3, fill=1, stroke=0)
    pdf.setFont("Helvetica-Bold", 9); pdf.drawString(83, height - 48, "FINCH / SYSTEMS & READING")
    if page == 1:
        pdf.setFillColor(NAVY); pdf.setFont("Helvetica-Bold", 28)
        pdf.drawString(44, height - 105, "Finch: Local-first")
        pdf.drawString(44, height - 139, "Paper Reading")
        pdf.setFont("Helvetica", 10.5); pdf.setFillColor(MUTED)
        pdf.drawString(44, height - 164, "Finch contributors  |  Engineering demonstration  |  Revision 1")
        pdf.setFillColor(PALE); pdf.roundRect(44, height - 303, width - 88, 111, 6, fill=1, stroke=0)
        paragraph(pdf, "ABSTRACT", 57, height - 208, width - 114, HEAD)
        paragraph(pdf, "A local paper reader should recover attention as carefully as it recovers a file. This original demonstration describes a compact architecture for responsive PDF reading: prioritize the visible page, preserve a stable reading anchor, account for caches, and start optional capabilities only when requested. It is a reading fixture, not a report of measured performance.", 57, height - 233, width - 114, SMALL)
        top = height - 332
    else:
        pdf.setFillColor(NAVY); pdf.setFont("Helvetica-Bold", 21 if width > 450 else 18)
        pdf.drawString(44, height - 92, title)
        pdf.setFillColor(MUTED); pdf.setFont("Helvetica", 10)
        pdf.drawString(44, height - 113, subtitle)
        top = height - 148
    columns = 1 if width < 450 else 2
    gap = 24; column_width = (width - 88 - gap * (columns - 1)) / columns
    style = SMALL if columns == 1 else BODY
    y = paragraph(pdf, f"{page}.1 &nbsp; {title}", 44, top, column_width, HEAD)
    y = paragraph(pdf, first, 44, y, column_width, style)
    y = paragraph(pdf, second, 44, y, column_width, style)
    if columns == 2:
        right = 44 + column_width + gap
        yr = paragraph(pdf, "Reading as a sequence", right, top, column_width, HEAD)
        yr = paragraph(pdf, "A useful reading session combines small actions: locate a passage, compare a figure, search a term, and return to the argument. Each action has a different data need. The interface can remain compact when the work behind those actions is separated into modules with explicit ownership.", right, yr, column_width)
        yr = paragraph(pdf, "For this page, try selecting the word <b>Finch</b>, changing the zoom, and then following a bookmark. Reopen the application after closing it. A repeatable fixture makes it possible to compare the same interaction before and after an implementation change.", right, yr, column_width)
        yr = paragraph(pdf, "<b>Design note.</b> The statements here describe desired behavior and test methods. They are not evidence that latency, memory, or frame-rate budgets have already been met.", right, yr, column_width, SMALL)
        bottom = min(y, yr) - 24
    else:
        bottom = y - 20
    if bottom > 145:
        diagram(pdf, 44, bottom - 44, width - 88, ["REQUEST", "VISIBLE WORK", "READING STATE"])
        pdf.setFont("Times-Italic", 9); pdf.setFillColor(MUTED)
        pdf.drawString(44, bottom - 61, f"Figure {page}. A conceptual reading pipeline; no measured values are implied.")
        bottom -= 91
    if bottom > 100:
        pdf.setFillColor(PALE); pdf.rect(44, bottom - 32, width - 88, 31, fill=1, stroke=0)
        pdf.setFont("Courier", 10); pdf.setFillColor(NAVY)
        pdf.drawString(55, bottom - 21, "work(t) = visible(t) + bounded_prefetch(t)" if width > 450 else "state = page + anchor + zoom")
    if page == 1:
        pdf.setFont("Helvetica", 9); pdf.setFillColor(TEAL)
        pdf.drawString(44, 69, "Explore the wide-page section (page 5)")
        pdf.linkAbsolute("Open the wide-page section", "page-5", Rect=(44, 65, 250, 81), thickness=0)
        pdf.drawRightString(width - 44, 69, "Illustrative external link: example.org")
        pdf.linkURL("https://example.org/finch-demo", (width - 229, 65, width - 44, 81), relative=0, thickness=0)
    footer(pdf, width, page)
    pdf.showPage()
pdf.save()
print(OUTPUT)
