//! Spreadsheets as grids of text: Excel workbooks (`xlsx`, `xlsm`, `xlsb`),
//! OpenDocument spreadsheets (`ods`) and delimited text (`csv`, `tsv`).
//!
//! Cells keep their sheet coordinates, so row 1 and column A are where the
//! spreadsheet program puts them. Rows past [`ROW_LIMIT`] and columns past
//! [`COLUMN_LIMIT`] are left out and the sheet is marked as cut.

use std::io::Cursor;

use calamine::{DataRef, Reader as _, SheetType, Xlsb, Xlsx};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use super::archive::Archive;
use super::{DocumentError, shorten, xml};

/// Rows read of a sheet at most.
pub const ROW_LIMIT: usize = 10_000;
/// Columns read of a sheet at most.
pub const COLUMN_LIMIT: usize = 256;
/// Characters kept of a cell.
const CELL_LIMIT: usize = 1_000;
/// Sheets read of a workbook at most.
const SHEET_LIMIT: usize = 64;

/// One sheet: rows of cells, each row as long as its last filled cell.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sheet {
    pub name: String,
    pub rows: Vec<Vec<String>>,
    /// Rows or columns beyond the limits were left out.
    pub truncated: bool,
}

impl Sheet {
    /// The number of columns: the length of the longest row.
    pub fn columns(&self) -> usize {
        self.rows.iter().map(Vec::len).max().unwrap_or(0)
    }

    fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Puts `text` at `row`, `column`. Returns `false` once `row` is past the
    /// limit, so readers going row by row can stop.
    fn set(&mut self, row: usize, column: usize, text: String) -> bool {
        if row >= ROW_LIMIT {
            self.truncated = true;
            return false;
        }
        if column >= COLUMN_LIMIT {
            self.truncated = true;
            return true;
        }
        if text.is_empty() {
            return true;
        }
        if self.rows.len() <= row {
            self.rows.resize_with(row + 1, Vec::new);
        }
        let cells = &mut self.rows[row];
        if cells.len() <= column {
            cells.resize_with(column + 1, String::new);
        }
        cells[column] = shorten(text, CELL_LIMIT);
        true
    }
}

/// The workbook formats read here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkbookFormat {
    /// Excel 2007 and later (`xlsx`, `xlsm`).
    Xlsx,
    /// Excel binary workbook (`xlsb`).
    Xlsb,
    /// OpenDocument spreadsheet (`ods`).
    Ods,
}

/// The sheets of a workbook.
pub fn workbook(bytes: &[u8], format: WorkbookFormat) -> Result<Vec<Sheet>, DocumentError> {
    let archive = Archive::open(bytes)?;
    let sheets = match format {
        WorkbookFormat::Xlsx => xlsx(bytes)?,
        WorkbookFormat::Xlsb => xlsb(bytes)?,
        WorkbookFormat::Ods => opendocument(archive)?,
    };
    if sheets.is_empty() {
        return Err(DocumentError::Damaged);
    }
    Ok(sheets)
}

/// Reads the worksheets of an Excel workbook of type `$workbook`. Cells
/// stream in sheet order, so reading stops at the limit; nothing is sized by
/// what the file claims about itself.
macro_rules! excel_reader {
    ($function:ident, $workbook:ident) => {
        fn $function(bytes: &[u8]) -> Result<Vec<Sheet>, DocumentError> {
            let mut workbook =
                $workbook::new(Cursor::new(bytes)).map_err(|error| excel_error(&error))?;
            let names: Vec<String> = workbook
                .sheets_metadata()
                .iter()
                .filter(|sheet| sheet.typ == SheetType::WorkSheet)
                .take(SHEET_LIMIT)
                .map(|sheet| sheet.name.clone())
                .collect();
            let mut sheets = Vec::with_capacity(names.len());
            for name in names {
                let mut sheet = Sheet::new(name);
                let mut cells = workbook
                    .worksheet_cells_reader(&sheet.name)
                    .map_err(|error| excel_error(&error))?;
                while let Some(cell) = cells.next_cell().map_err(|error| excel_error(&error))? {
                    let (row, column) = cell.get_position();
                    if !sheet.set(row as usize, column as usize, cell_text(cell.get_value())) {
                        break;
                    }
                }
                sheets.push(sheet);
            }
            Ok(sheets)
        }
    };
}

excel_reader!(xlsx, Xlsx);
excel_reader!(xlsb, Xlsb);

fn excel_error(error: &dyn std::fmt::Display) -> DocumentError {
    tracing::debug!(%error, "unreadable workbook");
    DocumentError::Damaged
}

/// The text of a cell, as a spreadsheet program shows it without formats.
fn cell_text(value: &DataRef<'_>) -> String {
    match value {
        DataRef::Empty => String::new(),
        DataRef::String(text) | DataRef::DateTimeIso(text) | DataRef::DurationIso(text) => {
            text.clone()
        }
        DataRef::SharedString(text) => (*text).to_owned(),
        DataRef::Int(number) => number.to_string(),
        DataRef::Float(number) => format_number(*number),
        DataRef::Bool(true) => "TRUE".to_owned(),
        DataRef::Bool(false) => "FALSE".to_owned(),
        DataRef::DateTime(date) => format_date(date),
        DataRef::Error(error) => error.to_string(),
    }
}

/// A number without the noise of binary floating point.
fn format_number(number: f64) -> String {
    let magnitude = number.abs();
    if !number.is_finite() {
        return number.to_string();
    }
    if magnitude >= 1e15 || (magnitude < 1e-9 && magnitude > 0.0) {
        return format!("{number:e}");
    }
    let text = format!("{number:.10}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text == "-0" { "0" } else { text }.to_owned()
}

fn format_date(date: &calamine::ExcelDateTime) -> String {
    if date.is_duration() {
        // Days as a float; shown as hours, minutes and seconds.
        let seconds = (date.as_f64() * 86_400.0).round();
        #[expect(
            clippy::cast_possible_truncation,
            reason = "durations in a sheet are far below i64::MAX seconds"
        )]
        let seconds = seconds as i64;
        let sign = if seconds < 0 { "-" } else { "" };
        let seconds = seconds.unsigned_abs();
        return format!(
            "{sign}{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        );
    }
    let (year, month, day, hour, minute, second, _) = date.to_ymd_hms_milli();
    let time = (hour, minute, second) != (0, 0, 0);
    if date.as_f64() < 1.0 && time {
        format!("{hour:02}:{minute:02}:{second:02}")
    } else if time {
        format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
    } else {
        format!("{year:04}-{month:02}-{day:02}")
    }
}

/// Reads an OpenDocument spreadsheet's `content.xml`.
fn opendocument(mut archive: Archive<'_>) -> Result<Vec<Sheet>, DocumentError> {
    let content = archive.text("content.xml")?.ok_or(DocumentError::Damaged)?;
    let mut reader = Reader::from_str(&content);
    let mut tables = Tables::default();
    loop {
        match reader.read_event().map_err(|error| xml::damaged(&error))? {
            Event::Start(element) => tables.start(&element),
            Event::Empty(element) => tables.empty(&element),
            Event::End(element) => tables.end(xml::name(element.name())),
            Event::Text(text) => tables.text(&text.xml10_content()),
            Event::GeneralRef(reference) => {
                if let Some(text) = xml::reference(&reference) {
                    tables.text(&text);
                }
            }
            Event::Eof => return Ok(tables.sheets),
            _ => {}
        }
    }
}

/// The tables of an OpenDocument spreadsheet, gathered event by event.
/// Repeated rows and cells are expanded only up to the limits.
#[derive(Default)]
struct Tables {
    sheets: Vec<Sheet>,
    /// Where the next row and cell go.
    row: usize,
    column: usize,
    /// How often the current row and cell repeat.
    row_repeat: usize,
    cell_repeat: usize,
    /// The text of the current cell, while in one.
    cell: Option<String>,
    paragraphs: usize,
    /// The filled cells of the current row, by column.
    row_cells: Vec<(usize, String)>,
    /// In a table past [`SHEET_LIMIT`].
    ignoring: bool,
}

impl Tables {
    fn start(&mut self, element: &BytesStart<'_>) {
        match xml::name(element.name()) {
            "table" => self.start_table(element),
            "table-row" => {
                self.row_repeat = repeats(element, "number-rows-repeated");
                self.column = 0;
                self.row_cells.clear();
            }
            "table-cell" | "covered-table-cell" => {
                self.cell_repeat = repeats(element, "number-columns-repeated");
                self.cell = Some(String::new());
                self.paragraphs = 0;
            }
            "p" => {
                if let Some(cell) = &mut self.cell {
                    if self.paragraphs > 0 {
                        cell.push('\n');
                    }
                    self.paragraphs += 1;
                }
            }
            _ => {}
        }
    }

    fn empty(&mut self, element: &BytesStart<'_>) {
        match xml::name(element.name()) {
            "table" => self.start_table(element),
            // Empty rows and cells only take up room.
            "table-row" => {
                self.row = self
                    .row
                    .saturating_add(repeats(element, "number-rows-repeated"));
            }
            "table-cell" | "covered-table-cell" => {
                self.column = self
                    .column
                    .saturating_add(repeats(element, "number-columns-repeated"));
            }
            "s" => {
                let count = xml::number_attr(element, "c").unwrap_or(1).min(64);
                self.text(&" ".repeat(count));
            }
            "tab" => self.text("\t"),
            "line-break" => self.text("\n"),
            _ => {}
        }
    }

    fn end(&mut self, name: &str) {
        match name {
            "table-row" => self.end_row(),
            "table-cell" | "covered-table-cell" => self.end_cell(),
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        if let Some(cell) = &mut self.cell
            && cell.len() < CELL_LIMIT * 4
        {
            cell.push_str(text);
        }
    }

    fn start_table(&mut self, element: &BytesStart<'_>) {
        self.ignoring = self.sheets.len() == SHEET_LIMIT;
        if !self.ignoring {
            self.sheets
                .push(Sheet::new(xml::attr(element, "name").unwrap_or_default()));
            self.row = 0;
        }
    }

    fn end_cell(&mut self) {
        let text = self.cell.take().unwrap_or_default();
        if !text.is_empty() {
            let room = COLUMN_LIMIT.saturating_sub(self.column);
            if self.cell_repeat > room
                && let Some(sheet) = self.sheets.last_mut()
            {
                sheet.truncated = true;
            }
            for offset in 0..self.cell_repeat.min(room) {
                self.row_cells.push((self.column + offset, text.clone()));
            }
        }
        self.column = self.column.saturating_add(self.cell_repeat);
    }

    fn end_row(&mut self) {
        if self.ignoring {
            return;
        }
        let Some(sheet) = self.sheets.last_mut() else {
            return;
        };
        if self.row_cells.is_empty() {
            self.row = self.row.saturating_add(self.row_repeat);
            return;
        }
        let room = ROW_LIMIT.saturating_sub(self.row);
        sheet.truncated |= self.row_repeat > room;
        let repeat = self.row_repeat.min(room).max(1);
        for offset in 0..repeat {
            for (column, text) in &self.row_cells {
                sheet.set(self.row + offset, *column, text.clone());
            }
        }
        self.row = self.row.saturating_add(repeat);
    }
}

/// A repeat count: at least 1, whatever the attribute says.
fn repeats(element: &BytesStart<'_>, attribute: &str) -> usize {
    xml::number_attr(element, attribute).unwrap_or(1).max(1)
}

/// Reads delimited text. Without a `delimiter`, the first line decides
/// between comma, semicolon and tab.
pub fn delimited(text: &str, delimiter: Option<char>, truncated: bool) -> Sheet {
    let delimiter = delimiter.unwrap_or_else(|| sniff(text));
    let mut sheet = Sheet::new("");
    sheet.truncated = truncated;
    let mut chars = text.chars().peekable();
    let (mut row, mut column) = (0, 0);
    let mut field = String::new();
    let mut quoted = false;
    let mut at_start = true;
    while let Some(character) = chars.next() {
        if quoted {
            match character {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => quoted = false,
                _ => field.push(character),
            }
            continue;
        }
        match character {
            '"' if at_start => {
                quoted = true;
                at_start = false;
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\r' => {
                if !sheet.set(row, column, std::mem::take(&mut field)) {
                    return sheet;
                }
                (row, column) = (row + 1, 0);
                at_start = true;
            }
            _ if character == delimiter => {
                sheet.set(row, column, std::mem::take(&mut field));
                column += 1;
                at_start = true;
            }
            _ => {
                field.push(character);
                at_start = false;
            }
        }
    }
    if !field.is_empty() {
        sheet.set(row, column, field);
    }
    sheet
}

/// The delimiter that splits the first line most often, outside quotes.
fn sniff(text: &str) -> char {
    let line = text.lines().next().unwrap_or_default();
    let mut counts = [(',', 0), (';', 0), ('\t', 0)];
    let mut quoted = false;
    for character in line.chars() {
        if character == '"' {
            quoted = !quoted;
        } else if !quoted && let Some(count) = counts.iter_mut().find(|(c, _)| *c == character) {
            count.1 += 1;
        }
    }
    counts
        .iter()
        .max_by_key(|(_, count)| *count)
        .filter(|(_, count)| *count > 0)
        .map_or(',', |(delimiter, _)| *delimiter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::archive::tests::zip;

    fn rows(sheet: &Sheet) -> Vec<Vec<&str>> {
        sheet
            .rows
            .iter()
            .map(|row| row.iter().map(String::as_str).collect())
            .collect()
    }

    #[test]
    fn reads_delimited_text() {
        let sheet = delimited(
            "name,size\r\n\"a, b\",1\n\"say \"\"hi\"\"\",2\n",
            None,
            false,
        );
        assert_eq!(
            rows(&sheet),
            [
                vec!["name", "size"],
                vec!["a, b", "1"],
                vec!["say \"hi\"", "2"]
            ]
        );
        assert!(!sheet.truncated);
    }

    #[test]
    fn finds_the_delimiter() {
        assert_eq!(sniff("a;b;c\n1,5;2;3"), ';');
        assert_eq!(sniff("a\tb\tc"), '\t');
        assert_eq!(sniff("\"x;y\",b,c"), ',');
        assert_eq!(sniff("single"), ',');
        let sheet = delimited("Name;Preis\nKäse;1,50\n", None, false);
        assert_eq!(rows(&sheet), [vec!["Name", "Preis"], vec!["Käse", "1,50"]]);
    }

    #[test]
    fn stops_at_the_row_limit() {
        let text = "x\n".repeat(ROW_LIMIT + 5);
        let sheet = delimited(&text, None, false);
        assert_eq!(sheet.rows.len(), ROW_LIMIT);
        assert!(sheet.truncated);
    }

    #[test]
    fn formats_numbers_plainly() {
        assert_eq!(format_number(3.0), "3");
        assert_eq!(format_number(0.1 + 0.2), "0.3");
        assert_eq!(format_number(-2.5), "-2.5");
        assert_eq!(format_number(-0.0), "0");
        assert_eq!(format_number(1e20), "1e20");
    }

    fn ods(content: &str) -> Vec<u8> {
        zip(&[
            (
                "mimetype",
                b"application/vnd.oasis.opendocument.spreadsheet",
            ),
            ("content.xml", content.as_bytes()),
        ])
    }

    #[test]
    fn reads_opendocument_spreadsheets() {
        let content = r#"<office:document-content
            xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
            xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
            xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
          <office:body><office:spreadsheet>
            <table:table table:name="Kosten">
              <table:table-row>
                <table:table-cell><text:p>Posten</text:p></table:table-cell>
                <table:table-cell/>
                <table:table-cell office:value-type="float" office:value="1.5"><text:p>1,50 €</text:p></table:table-cell>
              </table:table-row>
              <table:table-row table:number-rows-repeated="2">
                <table:table-cell table:number-columns-repeated="2"><text:p>x &amp; y</text:p></table:table-cell>
              </table:table-row>
              <table:table-row table:number-rows-repeated="1048570">
                <table:table-cell table:number-columns-repeated="16384"/>
              </table:table-row>
            </table:table>
            <table:table table:name="Leer"/>
          </office:spreadsheet></office:body>
        </office:document-content>"#;
        let sheets = workbook(&ods(content), WorkbookFormat::Ods).unwrap();
        assert_eq!(sheets.len(), 2);
        assert_eq!(sheets[0].name, "Kosten");
        assert_eq!(
            rows(&sheets[0]),
            [
                vec!["Posten", "", "1,50 €"],
                vec!["x & y", "x & y"],
                vec!["x & y", "x & y"]
            ]
        );
        assert!(!sheets[0].truncated);
        assert_eq!(sheets[1], Sheet::new("Leer"));
    }

    #[test]
    fn caps_repeats_in_opendocument_spreadsheets() {
        let content = r#"<document-content><body><spreadsheet><table name="Big">
            <table-row number-rows-repeated="1000000000">
              <table-cell number-columns-repeated="1000000000"><p>x</p></table-cell>
            </table-row>
        </table></spreadsheet></body></document-content>"#;
        let sheets = workbook(&ods(content), WorkbookFormat::Ods).unwrap();
        assert_eq!(sheets[0].rows.len(), ROW_LIMIT);
        assert_eq!(sheets[0].columns(), COLUMN_LIMIT);
        assert!(sheets[0].truncated);
    }
}
