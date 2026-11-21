use std::io::{Cursor, Read};

use indexfox_core::{normalize_search_text, ExtractedDocument, Origin, Section, SourceLocator};
use quick_xml::events::Event;
use quick_xml::Reader;
use zip::ZipArchive;

use crate::text::chunk_spans;
use crate::{ChunkOptions, ExtractError, Result, EXTRACTOR_VERSION};

/// Extract body paragraphs and table cells from OOXML.
///
/// Text inside all `w:t` runs of a paragraph is concatenated before matching,
/// so words split across runs still hit. Only `w:t` content is emitted:
/// field instructions, deleted text and drawings are ignored.
pub fn extract_docx(bytes: &[u8], options: ChunkOptions) -> Result<ExtractedDocument> {
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    let mut xml = Vec::new();
    match archive.by_name("word/document.xml") {
        Ok(mut file) => {
            file.read_to_end(&mut xml)?;
        }
        Err(zip::result::ZipError::FileNotFound) => return Err(ExtractError::MissingDocumentXml),
        Err(error) => return Err(error.into()),
    }
    parse_document_xml(&xml, options)
}

fn parse_document_xml(xml: &[u8], options: ChunkOptions) -> Result<ExtractedDocument> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();

    let mut sections = Vec::new();
    let mut paragraph: u32 = 0;
    let mut table: u32 = 0;
    let mut row: u32 = 0;
    let mut column: u32 = 0;
    let mut cell_depth: u32 = 0;
    let mut cell_start_paragraph: u32 = 0;
    let mut current_paragraph = String::new();
    let mut current_cell = String::new();
    let mut in_text = false;

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(event)) => match local_name(event.name().as_ref()) {
                b"p" => {
                    paragraph += 1;
                    current_paragraph.clear();
                }
                b"t" => in_text = true,
                b"tab" => current_paragraph.push('\t'),
                b"br" | b"cr" => current_paragraph.push('\n'),
                b"tbl" => {
                    table += 1;
                    row = 0;
                }
                b"tr" => {
                    row += 1;
                    column = 0;
                }
                b"tc" => {
                    if cell_depth == 0 {
                        cell_start_paragraph = paragraph + 1;
                    }
                    cell_depth += 1;
                    column += 1;
                    current_cell.clear();
                }
                _ => {}
            },
            Ok(Event::Empty(event)) => match local_name(event.name().as_ref()) {
                b"tab" => current_paragraph.push('\t'),
                b"br" | b"cr" => current_paragraph.push('\n'),
                _ => {}
            },
            Ok(Event::Text(event)) => {
                if in_text {
                    current_paragraph.push_str(&event.unescape()?);
                }
            }
            Ok(Event::End(event)) => match local_name(event.name().as_ref()) {
                b"t" => in_text = false,
                b"p" => {
                    if !current_paragraph.trim().is_empty() {
                        if cell_depth > 0 {
                            current_cell.push_str(&current_paragraph);
                            current_cell.push('\n');
                        } else {
                            push_paragraph_sections(
                                &mut sections,
                                paragraph,
                                &current_paragraph,
                                options,
                            );
                        }
                    }
                    current_paragraph.clear();
                }
                b"tc" => {
                    cell_depth = cell_depth.saturating_sub(1);
                    if cell_depth == 0 && !current_cell.trim().is_empty() {
                        push_cell_sections(
                            &mut sections,
                            table.max(1),
                            row.max(1),
                            column.max(1),
                            cell_start_paragraph,
                            &current_cell,
                            options,
                        );
                        current_cell.clear();
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => return Err(error.into()),
        }
        buffer.clear();
    }

    Ok(ExtractedDocument {
        extractor_version: EXTRACTOR_VERSION.to_string(),
        encoding: Some("ooxml".to_string()),
        sections,
    })
}

fn push_paragraph_sections(
    sections: &mut Vec<Section>,
    paragraph: u32,
    text: &str,
    options: ChunkOptions,
) {
    for span in chunk_spans(text, options) {
        let display = text[span.start..span.end].to_string();
        sections.push(Section {
            text: normalize_search_text(&display),
            display_text: display,
            locator: SourceLocator::DocxParagraph { paragraph },
            origin: Origin::Native,
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn push_cell_sections(
    sections: &mut Vec<Section>,
    table: u32,
    row: u32,
    column: u32,
    paragraph: u32,
    text: &str,
    options: ChunkOptions,
) {
    for span in chunk_spans(text, options) {
        let display = text[span.start..span.end].to_string();
        sections.push(Section {
            text: normalize_search_text(&display),
            display_text: display,
            locator: SourceLocator::DocxTableCell {
                table,
                row,
                column,
                paragraph,
            },
            origin: Origin::Native,
        });
    }
}

fn local_name(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|&byte| byte == b':') {
        Some(index) => &name[index + 1..],
        None => name,
    }
}
