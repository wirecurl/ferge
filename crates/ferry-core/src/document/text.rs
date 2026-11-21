//! Word processor and presentation files as structured text: headings,
//! paragraphs, list items, tables and slides. Layout, fonts and pictures are
//! left out; the point is to read what a document says.
//!
//! Read are Office Open XML (`docx`, `pptx`) and OpenDocument (`odt`,
//! `odp`). Output stops at [`BLOCK_LIMIT`] blocks or [`CHAR_LIMIT`]
//! characters.

use std::collections::HashMap;

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use super::archive::Archive;
use super::{DocumentError, shorten, xml};

/// Blocks read at most.
pub const BLOCK_LIMIT: usize = 20_000;
/// Characters read at most, over all blocks.
pub const CHAR_LIMIT: usize = 4 * 1024 * 1024;
/// Characters kept of a single paragraph.
const PARAGRAPH_LIMIT: usize = 20_000;
/// Rows and columns kept of a table.
const TABLE_ROW_LIMIT: usize = 1_000;
const TABLE_COLUMN_LIMIT: usize = 64;
/// Characters kept of a table cell.
const CELL_LIMIT: usize = 2_000;

/// A part of a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    /// A heading; level 1 is the topmost.
    Heading {
        level: u8,
        text: String,
    },
    Paragraph(String),
    /// An item of a list; depth 0 is the outermost list.
    Item {
        depth: u8,
        text: String,
    },
    /// Rows of cells.
    Table(Vec<Vec<String>>),
    /// The start of a slide, numbered from 1.
    Slide(usize),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Document {
    pub blocks: Vec<Block>,
    /// The document goes on beyond the limits.
    pub truncated: bool,
}

/// The formats read here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextFormat {
    /// Word 2007 and later (`docx`, `docm`).
    Docx,
    /// PowerPoint 2007 and later (`pptx`, `pptm`).
    Pptx,
    /// OpenDocument text (`odt`).
    Odt,
    /// OpenDocument presentation (`odp`).
    Odp,
}

/// The text of a document.
pub fn document(bytes: &[u8], format: TextFormat) -> Result<Document, DocumentError> {
    let mut archive = Archive::open(bytes)?;
    let mut collector = Collector::default();
    match format {
        TextFormat::Docx => word(&mut archive, &mut collector)?,
        TextFormat::Pptx => powerpoint(&mut archive, &mut collector)?,
        TextFormat::Odt | TextFormat::Odp => opendocument(&mut archive, &mut collector)?,
    }
    Ok(collector.finish())
}

/// What a paragraph turns into.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Kind {
    #[default]
    Plain,
    Heading(u8),
    Item(u8),
}

#[derive(Default)]
struct Paragraph {
    text: String,
    kind: Kind,
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<String>>,
    row: Option<Vec<String>>,
    cell: Option<String>,
}

/// Gathers blocks from the events of a walk through a document, within the
/// limits. Paragraphs nest (text boxes sit inside paragraphs); tables inside
/// tables become text of the outer table's cell.
#[derive(Default)]
struct Collector {
    blocks: Vec<Block>,
    chars: usize,
    truncated: bool,
    paragraphs: Vec<Paragraph>,
    table: Option<Table>,
    table_depth: usize,
}

impl Collector {
    fn full(&self) -> bool {
        self.blocks.len() >= BLOCK_LIMIT || self.chars >= CHAR_LIMIT
    }

    fn push(&mut self, block: Block) {
        if self.full() {
            self.truncated = true;
            return;
        }
        self.chars += match &block {
            Block::Heading { text, .. } | Block::Paragraph(text) | Block::Item { text, .. } => {
                text.len()
            }
            Block::Table(rows) => rows.iter().flatten().map(String::len).sum(),
            Block::Slide(_) => 0,
        };
        self.blocks.push(block);
    }

    fn start_paragraph(&mut self) {
        self.paragraphs.push(Paragraph::default());
    }

    fn set_kind(&mut self, kind: Kind) {
        if let Some(paragraph) = self.paragraphs.last_mut() {
            paragraph.kind = kind;
        }
    }

    fn text(&mut self, text: &str) {
        if let Some(paragraph) = self.paragraphs.last_mut()
            && paragraph.text.len() < PARAGRAPH_LIMIT * 4
        {
            paragraph.text.push_str(text);
        }
    }

    fn end_paragraph(&mut self) {
        let Some(paragraph) = self.paragraphs.pop() else {
            return;
        };
        let text = shorten(collapse(&paragraph.text), PARAGRAPH_LIMIT);
        if text.is_empty() {
            return;
        }
        if self.table_depth > 0 {
            if let Some(cell) = self.table.as_mut().and_then(|table| table.cell.as_mut()) {
                if !cell.is_empty() {
                    cell.push('\n');
                }
                cell.push_str(&text);
            }
            return;
        }
        self.push(match paragraph.kind {
            Kind::Plain => Block::Paragraph(text),
            Kind::Heading(level) => Block::Heading { level, text },
            Kind::Item(depth) => Block::Item { depth, text },
        });
    }

    fn start_table(&mut self) {
        self.table_depth += 1;
        if self.table_depth == 1 {
            self.table = Some(Table::default());
        }
    }

    fn end_table(&mut self) {
        self.table_depth = self.table_depth.saturating_sub(1);
        if self.table_depth == 0
            && let Some(table) = self.table.take()
            && !table.rows.is_empty()
        {
            self.push(Block::Table(table.rows));
        }
    }

    fn start_row(&mut self) {
        if self.table_depth == 1
            && let Some(table) = &mut self.table
        {
            table.row = Some(Vec::new());
        }
    }

    fn end_row(&mut self) {
        if self.table_depth != 1 {
            return;
        }
        let Some(table) = &mut self.table else {
            return;
        };
        if let Some(row) = table.row.take() {
            if table.rows.len() < TABLE_ROW_LIMIT {
                table.rows.push(row);
            } else {
                self.truncated = true;
            }
        }
    }

    fn start_cell(&mut self) {
        if self.table_depth == 1
            && let Some(table) = &mut self.table
        {
            table.cell = Some(String::new());
        }
    }

    fn end_cell(&mut self) {
        if self.table_depth != 1 {
            return;
        }
        let Some(table) = &mut self.table else {
            return;
        };
        if let (Some(cell), Some(row)) = (table.cell.take(), &mut table.row)
            && row.len() < TABLE_COLUMN_LIMIT
        {
            row.push(shorten(cell, CELL_LIMIT));
        }
    }

    fn finish(mut self) -> Document {
        // Whatever a damaged document left open.
        while !self.paragraphs.is_empty() {
            self.end_paragraph();
        }
        Document {
            blocks: self.blocks,
            truncated: self.truncated,
        }
    }
}

/// Text with runs of spaces and tabs collapsed and line ends trimmed, as
/// paragraphs are shown.
fn collapse(text: &str) -> String {
    text.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// Reads events of `content` until its end, handing each to `handle`.
/// Elements `handle` answers `Skip` for are passed over with their content.
fn walk(content: &str, mut handle: impl FnMut(&Event<'_>) -> Flow) -> Result<(), DocumentError> {
    let mut reader = Reader::from_str(content);
    loop {
        let event = reader.read_event().map_err(|error| xml::damaged(&error))?;
        if matches!(event, Event::Eof) {
            return Ok(());
        }
        match handle(&event) {
            Flow::Go => {}
            Flow::Skip => {
                if let Event::Start(element) = &event {
                    let end = element.to_end().into_owned();
                    reader
                        .read_to_end(end.name())
                        .map_err(|error| xml::damaged(&error))?;
                }
            }
            Flow::Stop => return Ok(()),
        }
    }
}

enum Flow {
    Go,
    Skip,
    Stop,
}

/// Text, entity references and CDATA as text.
fn event_text(event: &Event<'_>) -> Option<String> {
    match event {
        Event::Text(text) => Some(text.xml10_content().into_owned()),
        Event::GeneralRef(reference) => xml::reference(reference),
        Event::CData(data) => Some(data.xml10_content().into_owned()),
        _ => None,
    }
}

// Word.

fn word(archive: &mut Archive<'_>, collector: &mut Collector) -> Result<(), DocumentError> {
    let content = archive
        .text("word/document.xml")?
        .ok_or(DocumentError::Damaged)?;
    let headings = match archive.text("word/styles.xml")? {
        Some(styles) => heading_styles(&styles)?,
        None => HashMap::new(),
    };
    // Inside a run, where tabs and breaks are text rather than settings.
    let mut runs = 0_usize;
    let mut in_text = false;
    walk(&content, |event| {
        if collector.full() {
            collector.truncated = true;
            return Flow::Stop;
        }
        match event {
            Event::Start(element) => match xml::name(element.name()) {
                // Older renderings of drawings repeat their text.
                "Fallback" | "del" | "instrText" => return Flow::Skip,
                "p" => collector.start_paragraph(),
                "r" => runs += 1,
                "t" => in_text = true,
                "tbl" => collector.start_table(),
                "tr" => collector.start_row(),
                "tc" => collector.start_cell(),
                _ => {}
            },
            Event::Empty(element) => match xml::name(element.name()) {
                "pStyle" => {
                    if let Some(&level) = xml::attr(element, "val").and_then(|id| headings.get(&id))
                    {
                        collector.set_kind(Kind::Heading(level));
                    }
                }
                "outlineLvl" => {
                    if let Some(level) = xml::number_attr(element, "val").filter(|&l| l < 6) {
                        collector.set_kind(Kind::Heading(level_from(level + 1)));
                    }
                }
                "ilvl" => {
                    let depth = xml::number_attr(element, "val").unwrap_or(0);
                    collector.set_kind(Kind::Item(level_from(depth)));
                }
                "tab" if runs > 0 => collector.text("\t"),
                "br" | "cr" if runs > 0 => collector.text("\n"),
                _ => {}
            },
            Event::End(element) => match xml::name(element.name()) {
                "p" => collector.end_paragraph(),
                "r" => runs = runs.saturating_sub(1),
                "t" => in_text = false,
                "tbl" => collector.end_table(),
                "tr" => collector.end_row(),
                "tc" => collector.end_cell(),
                _ => {}
            },
            _ if in_text => {
                if let Some(text) = event_text(event) {
                    collector.text(&text);
                }
            }
            _ => {}
        }
        Flow::Go
    })
}

/// The heading level of each paragraph style that is a heading, by style ID.
/// IDs are localized (`berschrift1` in German Word); names are not.
fn heading_styles(styles: &str) -> Result<HashMap<String, u8>, DocumentError> {
    let mut headings = HashMap::new();
    let mut current: Option<String> = None;
    walk(styles, |event| {
        match event {
            Event::Start(element) if xml::name(element.name()) == "style" => {
                current = xml::attr(element, "styleId");
            }
            Event::End(element) if xml::name(element.name()) == "style" => current = None,
            Event::Empty(element) => {
                let Some(id) = &current else {
                    return Flow::Go;
                };
                let level = match xml::name(element.name()) {
                    "name" => xml::attr(element, "val").and_then(|name| heading_level(&name)),
                    "outlineLvl" => xml::number_attr(element, "val")
                        .filter(|&level| level < 6)
                        .map(|level| level_from(level + 1)),
                    _ => None,
                };
                if let Some(level) = level {
                    headings.insert(id.clone(), level);
                }
            }
            _ => {}
        }
        Flow::Go
    })?;
    Ok(headings)
}

/// The level of a built-in heading style by its name.
fn heading_level(name: &str) -> Option<u8> {
    let name = name.to_ascii_lowercase();
    if name == "title" {
        return Some(1);
    }
    let level: usize = name.strip_prefix("heading ")?.trim().parse().ok()?;
    (1..=9).contains(&level).then(|| level_from(level.min(6)))
}

fn level_from(value: usize) -> u8 {
    u8::try_from(value.min(9)).unwrap_or(9)
}

// PowerPoint.

fn powerpoint(archive: &mut Archive<'_>, collector: &mut Collector) -> Result<(), DocumentError> {
    let presentation = archive
        .text("ppt/presentation.xml")?
        .ok_or(DocumentError::Damaged)?;
    let relations = archive
        .text("ppt/_rels/presentation.xml.rels")?
        .ok_or(DocumentError::Damaged)?;
    let targets = relationship_targets(&relations)?;
    // Slides in the order the presentation lists them.
    let mut slides = Vec::new();
    walk(&presentation, |event| {
        if let Event::Empty(element) | Event::Start(element) = event
            && xml::name(element.name()) == "sldId"
            && let Some(target) = prefixed_attr(element, "id").and_then(|id| targets.get(&id))
        {
            slides.push(format!(
                "ppt/{}",
                target.trim_start_matches('/').trim_start_matches("ppt/")
            ));
        }
        Flow::Go
    })?;
    for (index, path) in slides.iter().enumerate() {
        if collector.full() {
            collector.truncated = true;
            break;
        }
        let Some(slide) = archive.text(path)? else {
            continue;
        };
        collector.push(Block::Slide(index + 1));
        slide_text(&slide, collector)?;
    }
    Ok(())
}

/// The text of one slide: its title as a heading, then the other shapes.
fn slide_text(slide: &str, collector: &mut Collector) -> Result<(), DocumentError> {
    let mut title = false;
    let mut in_text = false;
    walk(slide, |event| {
        match event {
            Event::Start(element) => match xml::name(element.name()) {
                "Fallback" => return Flow::Skip,
                "sp" => title = false,
                "p" => {
                    collector.start_paragraph();
                    if title {
                        collector.set_kind(Kind::Heading(2));
                    }
                }
                "pPr" => paragraph_level(element, title, collector),
                "t" => in_text = true,
                "tbl" => collector.start_table(),
                "tr" => collector.start_row(),
                "tc" => collector.start_cell(),
                _ => {}
            },
            Event::Empty(element) => match xml::name(element.name()) {
                "ph" => {
                    title = matches!(
                        xml::attr(element, "type").as_deref(),
                        Some("title" | "ctrTitle")
                    );
                }
                "pPr" => paragraph_level(element, title, collector),
                "br" => collector.text("\n"),
                _ => {}
            },
            Event::End(element) => match xml::name(element.name()) {
                "p" => collector.end_paragraph(),
                "t" => in_text = false,
                "tbl" => collector.end_table(),
                "tr" => collector.end_row(),
                "tc" => collector.end_cell(),
                _ => {}
            },
            _ if in_text => {
                if let Some(text) = event_text(event) {
                    collector.text(&text);
                }
            }
            _ => {}
        }
        Flow::Go
    })
}

/// Indented paragraphs of a slide are list items.
fn paragraph_level(element: &BytesStart<'_>, title: bool, collector: &mut Collector) {
    if let Some(level) = xml::number_attr(element, "lvl").filter(|&level| level > 0 && !title) {
        collector.set_kind(Kind::Item(level_from(level)));
    }
}

/// The targets of a relationships part, by relationship ID.
fn relationship_targets(relations: &str) -> Result<HashMap<String, String>, DocumentError> {
    let mut targets = HashMap::new();
    walk(relations, |event| {
        if let Event::Empty(element) | Event::Start(element) = event
            && xml::name(element.name()) == "Relationship"
            && let (Some(id), Some(target)) =
                (xml::attr(element, "Id"), xml::attr(element, "Target"))
        {
            targets.insert(id, target);
        }
        Flow::Go
    })?;
    Ok(targets)
}

/// The value of a namespaced attribute such as `r:id`, told apart from a
/// plain `id` on the same element.
fn prefixed_attr(element: &BytesStart<'_>, local: &str) -> Option<String> {
    element
        .attributes()
        .with_checks(false)
        .flatten()
        .find(|attribute| {
            attribute.key.prefix().is_some() && attribute.key.local_name().into_inner() == local
        })
        .map(|attribute| attribute.value.into_owned())
}

// OpenDocument.

fn opendocument(archive: &mut Archive<'_>, collector: &mut Collector) -> Result<(), DocumentError> {
    let content = archive.text("content.xml")?.ok_or(DocumentError::Damaged)?;
    let mut lists = 0_usize;
    let mut slides = 0_usize;
    // Frames that are slide titles, counted so nested frames end right.
    let mut title_frames = 0_usize;
    let mut frames: Vec<bool> = Vec::new();
    walk(&content, |event| {
        if collector.full() {
            collector.truncated = true;
            return Flow::Stop;
        }
        match event {
            Event::Start(element) => match xml::name(element.name()) {
                // Footnotes, comments, speaker notes and recorded changes.
                "note" | "annotation" | "notes" | "tracked-changes" => return Flow::Skip,
                "page" => {
                    slides += 1;
                    collector.push(Block::Slide(slides));
                }
                "frame" => {
                    let title = xml::attr(element, "class").as_deref() == Some("title");
                    title_frames += usize::from(title);
                    frames.push(title);
                }
                "h" => {
                    collector.start_paragraph();
                    let level = xml::number_attr(element, "outline-level").unwrap_or(1);
                    collector.set_kind(Kind::Heading(level_from(level.clamp(1, 6))));
                }
                "p" => {
                    collector.start_paragraph();
                    if title_frames > 0 {
                        collector.set_kind(Kind::Heading(2));
                    } else if lists > 0 {
                        collector.set_kind(Kind::Item(level_from(lists - 1)));
                    }
                }
                "list" => lists += 1,
                "table" => collector.start_table(),
                "table-row" => collector.start_row(),
                "table-cell" | "covered-table-cell" => collector.start_cell(),
                _ => {}
            },
            Event::Empty(element) => match xml::name(element.name()) {
                "s" => {
                    let count = xml::number_attr(element, "c").unwrap_or(1).min(64);
                    collector.text(&" ".repeat(count));
                }
                "tab" => collector.text("\t"),
                "line-break" => collector.text("\n"),
                _ => {}
            },
            Event::End(element) => match xml::name(element.name()) {
                "h" | "p" => collector.end_paragraph(),
                "list" => lists = lists.saturating_sub(1),
                "frame" => {
                    if frames.pop() == Some(true) {
                        title_frames -= 1;
                    }
                }
                "table" => collector.end_table(),
                "table-row" => collector.end_row(),
                "table-cell" | "covered-table-cell" => collector.end_cell(),
                _ => {}
            },
            _ => {
                if let Some(text) = event_text(event) {
                    collector.text(&text);
                }
            }
        }
        Flow::Go
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::archive::tests::zip;

    fn paragraph(text: &str) -> Block {
        Block::Paragraph(text.to_owned())
    }

    fn heading(level: u8, text: &str) -> Block {
        Block::Heading {
            level,
            text: text.to_owned(),
        }
    }

    fn item(depth: u8, text: &str) -> Block {
        Block::Item {
            depth,
            text: text.to_owned(),
        }
    }

    const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

    #[test]
    fn reads_word_documents() {
        let styles = format!(
            r#"<w:styles {W}>
                <w:style w:type="paragraph" w:styleId="berschrift1"><w:name w:val="heading 1"/></w:style>
                <w:style w:type="paragraph" w:styleId="Titel"><w:name w:val="Title"/></w:style>
            </w:styles>"#
        );
        let document = format!(
            r#"<w:document {W}><w:body>
                <w:p><w:pPr><w:pStyle w:val="Titel"/></w:pPr><w:r><w:t>Bericht</w:t></w:r></w:p>
                <w:p><w:pPr><w:pStyle w:val="berschrift1"/><w:tabs><w:tab w:val="left"/></w:tabs></w:pPr>
                    <w:r><w:t>Einleitung</w:t></w:r></w:p>
                <w:p><w:r><w:t xml:space="preserve">Kosten &amp; </w:t></w:r><w:r><w:t>Nutzen</w:t><w:tab/><w:t>ok</w:t></w:r>
                    <w:del><w:r><w:delText>weg</w:delText></w:r></w:del></w:p>
                <w:p><w:pPr><w:numPr><w:ilvl w:val="1"/></w:numPr></w:pPr><w:r><w:t>Punkt</w:t></w:r></w:p>
                <w:p/>
                <w:tbl><w:tr><w:tc><w:p><w:r><w:t>A</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>B</w:t></w:r></w:p>
                    <w:p><w:r><w:t>C</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
            </w:body></w:document>"#
        );
        let bytes = zip(&[
            ("word/document.xml", document.as_bytes()),
            ("word/styles.xml", styles.as_bytes()),
        ]);
        let document = super::document(&bytes, TextFormat::Docx).unwrap();
        assert_eq!(
            document.blocks,
            [
                heading(1, "Bericht"),
                heading(1, "Einleitung"),
                paragraph("Kosten & Nutzen ok"),
                item(1, "Punkt"),
                Block::Table(vec![vec!["A".to_owned(), "B\nC".to_owned()]]),
            ]
        );
        assert!(!document.truncated);
    }

    #[test]
    fn reads_presentations_in_slide_order() {
        let presentation = r#"<p:presentation xmlns:p="p" xmlns:r="r"><p:sldIdLst>
            <p:sldId id="257" r:id="rId3"/><p:sldId id="256" r:id="rId2"/>
        </p:sldIdLst></p:presentation>"#;
        let relations = r#"<Relationships>
            <Relationship Id="rId2" Target="slides/slide1.xml"/>
            <Relationship Id="rId3" Target="slides/slide2.xml"/>
        </Relationships>"#;
        let slide = |title: &str, body: &str| {
            format!(
                r#"<p:sld xmlns:p="p" xmlns:a="a"><p:cSld><p:spTree>
                <p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
                    <p:txBody><a:p><a:r><a:t>{title}</a:t></a:r></a:p></p:txBody></p:sp>
                <p:sp><p:nvSpPr><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr>
                    <p:txBody><a:p><a:r><a:t>{body}</a:t></a:r></a:p>
                    <a:p><a:pPr lvl="1"/><a:r><a:t>Detail</a:t></a:r></a:p></p:txBody></p:sp>
                </p:spTree></p:cSld></p:sld>"#
            )
        };
        let first = slide("Erste", "Hallo");
        let second = slide("Zweite", "Welt");
        let bytes = zip(&[
            ("ppt/presentation.xml", presentation.as_bytes()),
            ("ppt/_rels/presentation.xml.rels", relations.as_bytes()),
            ("ppt/slides/slide1.xml", first.as_bytes()),
            ("ppt/slides/slide2.xml", second.as_bytes()),
        ]);
        let document = super::document(&bytes, TextFormat::Pptx).unwrap();
        assert_eq!(
            document.blocks,
            [
                Block::Slide(1),
                heading(2, "Zweite"),
                paragraph("Welt"),
                item(1, "Detail"),
                Block::Slide(2),
                heading(2, "Erste"),
                paragraph("Hallo"),
                item(1, "Detail"),
            ]
        );
    }

    #[test]
    fn reads_opendocument_text() {
        let content = r#"<office:document-content xmlns:office="o" xmlns:text="t" xmlns:table="tb">
            <office:body><office:text>
                <text:h text:outline-level="2">Teil<text:s text:c="2"/>eins</text:h>
                <text:p>Hallo<text:note><text:note-body><text:p>Fußnote</text:p></text:note-body></text:note> Welt</text:p>
                <text:list><text:list-item><text:p>Eins</text:p>
                    <text:list><text:list-item><text:p>Unter</text:p></text:list-item></text:list>
                </text:list-item></text:list>
                <table:table><table:table-row><table:table-cell><text:p>Z</text:p></table:table-cell></table:table-row></table:table>
            </office:text></office:body></office:document-content>"#;
        let bytes = zip(&[("content.xml", content.as_bytes())]);
        let document = super::document(&bytes, TextFormat::Odt).unwrap();
        assert_eq!(
            document.blocks,
            [
                heading(2, "Teil eins"),
                paragraph("Hallo Welt"),
                item(0, "Eins"),
                item(1, "Unter"),
                Block::Table(vec![vec!["Z".to_owned()]]),
            ]
        );
    }

    #[test]
    fn reads_opendocument_presentations() {
        let content = r#"<office:document-content xmlns:office="o" xmlns:text="t" xmlns:draw="d" xmlns:presentation="p">
            <office:body><office:presentation>
                <draw:page><draw:frame presentation:class="title"><draw:text-box><text:p>Titel</text:p></draw:text-box></draw:frame>
                    <draw:frame presentation:class="outline"><draw:text-box><text:p>Inhalt</text:p></draw:text-box></draw:frame>
                    <presentation:notes><draw:frame><draw:text-box><text:p>Notiz</text:p></draw:text-box></draw:frame></presentation:notes>
                </draw:page>
            </office:presentation></office:body></office:document-content>"#;
        let bytes = zip(&[("content.xml", content.as_bytes())]);
        let document = super::document(&bytes, TextFormat::Odp).unwrap();
        assert_eq!(
            document.blocks,
            [Block::Slide(1), heading(2, "Titel"), paragraph("Inhalt")]
        );
    }

    #[test]
    fn stops_at_the_block_limit() {
        let paragraphs = "<w:p><w:r><w:t>x</w:t></w:r></w:p>".repeat(BLOCK_LIMIT + 10);
        let content = format!("<w:document {W}><w:body>{paragraphs}</w:body></w:document>");
        let bytes = zip(&[("word/document.xml", content.as_bytes())]);
        let document = super::document(&bytes, TextFormat::Docx).unwrap();
        assert_eq!(document.blocks.len(), BLOCK_LIMIT);
        assert!(document.truncated);
    }

    #[test]
    fn rejects_damaged_documents() {
        let bytes = zip(&[("word/document.xml", b"<w:document><w:body><w:p>")]);
        assert!(super::document(&bytes, TextFormat::Docx).is_ok());
        let bytes = zip(&[("other.xml", b"<a/>")]);
        assert_eq!(
            super::document(&bytes, TextFormat::Docx),
            Err(DocumentError::Damaged)
        );
    }
}
