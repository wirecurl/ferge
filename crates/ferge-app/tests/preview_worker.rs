//! The preview worker, run as the real `ferge` binary: documents are read in
//! a separate process, and a file that exhausts the worker's memory ends the
//! worker only, with a "too large" answer for the caller.

#![expect(
    clippy::unwrap_used,
    reason = "test fixtures; a failure is a test failure"
)]

use std::io::{Cursor, Write as _};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ferge_core::document::DocumentError;
use ferge_core::document::sheet::WorkbookFormat;
use ferge_core::document::{pdf::Pdf, worker};
use zip::write::{SimpleFileOptions, ZipWriter};

fn setup() {
    worker::set_program(PathBuf::from(env!("CARGO_BIN_EXE_ferge")));
}

fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, content) in entries {
        writer
            .start_file(*name, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(content).unwrap();
    }
    writer.finish().unwrap().into_inner()
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
fn reads_a_workbook_in_the_worker() {
    setup();
    let content = r#"<document-content><body><spreadsheet><table name="Tabelle">
        <table-row><table-cell><p>a</p></table-cell><table-cell><p>b</p></table-cell></table-row>
        </table></spreadsheet></body></document-content>"#;
    let sheets = worker::workbook(&ods(content), WorkbookFormat::Ods).unwrap();
    assert_eq!(sheets.len(), 1);
    assert_eq!(sheets[0].name, "Tabelle");
    assert_eq!(sheets[0].rows, [vec!["a".to_owned(), "b".to_owned()]]);
}

#[test]
fn a_memory_bomb_ends_only_the_worker() {
    setup();
    // calamine sets memory aside for the number of shared strings a
    // workbook declares: here 2.4 GB, which an unlimited process would get
    // (and then report the workbook as damaged). The declaration hides
    // behind a long comment, past what ferge checks beforehand, so only the
    // worker's memory limit stops it.
    let padding = "x".repeat(70 * 1024);
    let strings = format!(
        r#"<?xml version="1.0"?><!-- {padding} --><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" uniqueCount="100000000"><si><t>a</t></si></sst>"#
    );
    let relations = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;
    let bytes = zip(&[
        ("_rels/.rels", relations.as_bytes()),
        ("xl/sharedStrings.xml", strings.as_bytes()),
    ]);
    let started = Instant::now();
    assert_eq!(
        worker::workbook(&bytes, WorkbookFormat::Xlsx),
        Err(DocumentError::TooLarge)
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    // And the next preview works as before.
    reads_a_workbook_in_the_worker();
}

#[test]
fn damaged_files_are_reported_as_damaged() {
    setup();
    assert_eq!(
        worker::workbook(b"not a workbook", WorkbookFormat::Xlsx),
        Err(DocumentError::Damaged)
    );
    assert_eq!(
        Pdf::open(b"%PDF-1.4 nothing").err(),
        Some(DocumentError::Damaged)
    );
    assert_eq!(
        worker::picture(b"not a picture", false).err(),
        Some(DocumentError::Damaged)
    );
}
