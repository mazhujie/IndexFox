#!/usr/bin/env python3
"""Generate synthetic IndexFox test fixtures. No personal data, no network."""
from pathlib import Path
import zipfile

ROOT = Path(__file__).resolve().parent.parent / "fixtures"
TEXT = ROOT / "text"
DOCX = ROOT / "docx"
PDF = ROOT / "pdf"


def write_text_fixtures():
    TEXT.mkdir(parents=True, exist_ok=True)
    (TEXT / "kernel.md").write_text(
        "# Linux 内核驱动\n"
        "VIDIOC_STREAMON 采集卡寄存器说明\n"
        "dma_alloc_coherent() 分配一致性内存\n"
        "#include <linux/videodev2.h>\n"
        "进度 100% 名称 a_b\n",
        encoding="utf-8",
    )
    (TEXT / "code.c").write_text(
        "#include <linux/dma-mapping.h>\n"
        "static int start(struct video_device *vdev) {\n"
        "    return VIDIOC_STREAMON;\n"
        "}\n"
        "void *buf = dma_alloc_coherent(dev, size, &addr, GFP_KERNEL);\n"
        "/* empty-arg spelling: dma_alloc_coherent() */\n"
        "/* 100% 覆盖 a_b */\n",
        encoding="utf-8",
    )
    (TEXT / "gbk.txt").write_bytes("Linux 内核驱动 GBK 中文测试".encode("gbk"))
    (TEXT / "utf16.txt").write_bytes(b"\xff\xfe" + "Linux 内核驱动 UTF16 测试".encode("utf-16-le"))
    # One logical line longer than the 16 KiB hard chunk limit, with the phrase
    # straddling the boundary at byte 16384.
    boundary = "a" * 16381 + "内核驱动" + "b" * 100
    (TEXT / "cross_chunk.txt").write_text(boundary, encoding="utf-8")


def write_docx():
    DOCX.mkdir(parents=True, exist_ok=True)
    document = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p><w:r><w:t>VIDIOC_STREAM</w:t></w:r><w:r><w:t>ON</w:t></w:r><w:r><w:t xml:space="preserve"> 内核驱动段落</w:t></w:r></w:p>
    <w:tbl>
      <w:tr>
        <w:tc><w:p><w:r><w:t>寄存器 DMA_ADDR 0x1F</w:t></w:r></w:p></w:tc>
        <w:tc><w:p><w:r><w:t>表格单元 dma_alloc_coherent()</w:t></w:r></w:p></w:tc>
      </w:tr>
    </w:tbl>
    <w:p><w:r><w:t>tail paragraph plain</w:t></w:r></w:p>
  </w:body>
</w:document>
"""
    content_types = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>
"""
    rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>
"""
    with zipfile.ZipFile(DOCX / "cross_run_table.docx", "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("[Content_Types].xml", content_types)
        archive.writestr("_rels/.rels", rels)
        archive.writestr("word/document.xml", document)


def write_pdf():
    PDF.mkdir(parents=True, exist_ok=True)
    pages = [
        "IndexFox PDF fixture page one",
        "VIDIOC_STREAMON dma_alloc_coherent() page two",
        "Linux kernel driver page three",
    ]
    objects = []

    def add(body):
        objects.append(body)
        return len(objects)

    catalog = add("<< /Type /Catalog /Pages 2 0 R >>")
    assert catalog == 1
    add("<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 >>")
    content_ids = []
    for text in pages:
        escaped = text.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")
        stream = f"BT /F1 20 Tf 72 720 Td ({escaped}) Tj ET"
        content_ids.append(add(f"<< /Length {len(stream)} >>\nstream\n{stream}\nendstream"))
    font = add("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")
    for index, content_id in enumerate(content_ids):
        add(
            f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
            f"/Resources << /Font << /F1 {font} 0 R >> >> /Contents {content_id} 0 R >>"
        )
    # Pages were appended after content streams; rewrite Kids with real page ids.
    page_ids = [len(objects) - 2, len(objects) - 1, len(objects)]
    objects[1] = "<< /Type /Pages /Kids [" + " ".join(f"{p} 0 R" for p in page_ids) + "] /Count 3 >>"

    buffer = bytearray(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
    offsets = [0]
    for number, body in enumerate(objects, 1):
        offsets.append(len(buffer))
        buffer.extend(f"{number} 0 obj\n{body}\nendobj\n".encode("latin-1"))
    xref = len(buffer)
    buffer.extend(f"xref\n0 {len(objects) + 1}\n".encode())
    buffer.extend(b"0000000000 65535 f \n")
    for offset in offsets[1:]:
        buffer.extend(f"{offset:010d} 00000 n \n".encode())
    buffer.extend(
        f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
    )
    (PDF / "sample.pdf").write_bytes(bytes(buffer))


if __name__ == "__main__":
    write_text_fixtures()
    write_docx()
    write_pdf()
    print(f"fixtures written under {ROOT}")
