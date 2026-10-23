"""Optional fixture regeneration; requires PyMuPDF, not used by Finch at runtime."""
from pathlib import Path
import pymupdf

document = pymupdf.open()
page = document.new_page(width=600, height=800)
page.insert_text((72, 72), "Finch password protected paper")
document.set_metadata({"title": "Encrypted Finch fixture"})
document.save(
    Path(__file__).with_name("encrypted.pdf"),
    encryption=pymupdf.PDF_ENCRYPT_AES_256,
    user_pw="finch",
    owner_pw="fixture-owner-password",
    permissions=pymupdf.PDF_PERM_COPY | pymupdf.PDF_PERM_PRINT,
)
