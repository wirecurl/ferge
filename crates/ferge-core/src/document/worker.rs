//! Reading untrusted files in a separate process.
//!
//! Spreadsheets, office documents, PDF pages and pictures are decoded by
//! third-party parsers that a crafted file can drive into huge allocations,
//! endless loops, deep recursion or panics, and some of these (an allocation
//! failure, a stack overflow) end a process no matter what. So the preview
//! does not decode them itself: it starts ferge again as a worker (with the
//! hidden argument [`ARGUMENT`]), sends it the bytes and reads the result.
//! The worker may allocate at most [`MEMORY_LIMIT`] bytes (the binary's
//! allocator enforces it, see [`run`]) and is killed after [`TIMEOUT`]. If
//! it dies, the preview says the file is too large and ferge carries on.
//!
//! Messages are frames: a little-endian `u32` length, then the body (see
//! [`super::wire`]). A request starts with a tag; a reply with a status.

use std::io::{self, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::Duration;

use super::picture::{self, Frame, Picture};
use super::sheet::{self, Sheet, WorkbookFormat};
use super::text::{self, Block, Document, TextFormat};
use super::wire::{Reader, Writer};
use super::{Bitmap, DocumentError};

/// The command-line argument that makes ferge run as a worker.
pub const ARGUMENT: &str = "--preview-worker";
/// Memory a worker may allocate.
pub const MEMORY_LIMIT: usize = 1536 * 1024 * 1024;
/// How long a worker may take for one request.
pub const TIMEOUT: Duration = Duration::from_secs(30);
/// Frames larger than this are refused; nothing sent comes close.
const FRAME_LIMIT: usize = MEMORY_LIMIT;

const WORKBOOK: u8 = 1;
const TEXT: u8 = 2;
const PICTURE: u8 = 3;
const PDF_OPEN: u8 = 4;
const PDF_RENDER: u8 = 5;

const OK: u8 = 0;
const DAMAGED: u8 = 1;
const ENCRYPTED: u8 = 2;
const TOO_LARGE: u8 = 3;

/// The program started as a worker, set once at startup.
static PROGRAM: OnceLock<PathBuf> = OnceLock::new();

/// Makes `program` (normally ferge itself) the worker for previews.
pub fn set_program(program: PathBuf) {
    let _ = PROGRAM.set(program);
}

/// The program to start as a worker: the running executable. On Linux,
/// `/proc/self/exe` still works after the file was replaced by an update.
pub fn current_program() -> io::Result<PathBuf> {
    if cfg!(target_os = "linux") {
        Ok(PathBuf::from("/proc/self/exe"))
    } else {
        std::env::current_exe()
    }
}

// ---------------------------------------------------------------------------
// The worker side

/// Runs as a worker on stdin and stdout until the request is answered (or,
/// for a PDF, until stdin closes). Call it first thing in `main` when the
/// first argument is [`ARGUMENT`], before anything else is set up.
/// `limit_memory` must make allocations past the given number of bytes
/// fail, normally through the binary's global allocator; a failed
/// allocation ends the worker.
pub fn run(limit_memory: impl FnOnce(usize) -> bool) -> std::process::ExitCode {
    if !limit_memory(MEMORY_LIMIT) {
        return std::process::ExitCode::FAILURE;
    }
    // The parent reads the status, not messages on stderr.
    std::panic::set_hook(Box::new(|_| {}));
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    match serve(&mut input, &mut output) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(_) => std::process::ExitCode::FAILURE,
    }
}

/// Answers one request read from `input`; a PDF is then served page by
/// page until `input` ends.
pub(crate) fn serve(input: &mut impl Read, output: &mut impl Write) -> io::Result<()> {
    let request = read_frame(input)?;
    let mut reader = Reader::new(&request);
    let reply = match reader.u8() {
        Ok(WORKBOOK) => answer(|| {
            let format = workbook_format(reader.u8()?)?;
            let bytes = reader.bytes()?;
            sheet::workbook(bytes, format).map(|sheets| encode_sheets(&sheets))
        }),
        Ok(TEXT) => answer(|| {
            let format = text_format(reader.u8()?)?;
            let bytes = reader.bytes()?;
            text::document(bytes, format).map(|document| encode_document(&document))
        }),
        Ok(PICTURE) => answer(|| {
            let svg = reader.bool()?;
            let bytes = reader.bytes()?;
            picture::decode(bytes, svg).map(|picture| encode_picture(&picture))
        }),
        Ok(PDF_OPEN) => {
            let bytes = match reader.bytes() {
                Ok(bytes) => bytes::Bytes::copy_from_slice(bytes),
                Err(error) => return write_frame(output, &error_reply(error)),
            };
            drop(request);
            return super::pdf::serve_worker(bytes, input, output);
        }
        _ => error_reply(DocumentError::Damaged),
    };
    write_frame(output, &reply)
}

/// The reply for `work`, with panics in the parsers reported as damage.
fn answer(work: impl FnOnce() -> Result<Writer, DocumentError>) -> Vec<u8> {
    match catch_unwind(AssertUnwindSafe(work)) {
        Ok(Ok(body)) => ok_reply(body),
        Ok(Err(error)) => error_reply(error),
        Err(_) => error_reply(DocumentError::Damaged),
    }
}

/// The reply that reports `error`.
pub(crate) fn error_reply(error: DocumentError) -> Vec<u8> {
    vec![match error {
        DocumentError::Damaged => DAMAGED,
        DocumentError::Encrypted => ENCRYPTED,
        DocumentError::TooLarge => TOO_LARGE,
    }]
}

/// The reply with status OK and `body`.
pub(crate) fn ok_reply(body: Writer) -> Vec<u8> {
    let body = body.finish();
    let mut reply = Vec::with_capacity(body.len() + 1);
    reply.push(OK);
    reply.extend_from_slice(&body);
    reply
}

pub(crate) fn read_frame(input: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut length = [0_u8; 4];
    input.read_exact(&mut length)?;
    let length = usize::try_from(u32::from_le_bytes(length)).unwrap_or(usize::MAX);
    if length > FRAME_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = Vec::new();
    body.try_reserve_exact(length)
        .map_err(|_| io::Error::new(io::ErrorKind::OutOfMemory, "frame too large"))?;
    input.take(length as u64).read_to_end(&mut body)?;
    if body.len() != length {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(body)
}

pub(crate) fn write_frame(output: &mut impl Write, body: &[u8]) -> io::Result<()> {
    let length = u32::try_from(body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame too large"))?;
    output.write_all(&length.to_le_bytes())?;
    output.write_all(body)?;
    output.flush()
}

// ---------------------------------------------------------------------------
// The client side

/// A running worker.
pub(crate) struct Worker {
    child: Peer,
    input: Box<dyn Write + Send>,
    output: Box<dyn Read + Send>,
}

enum Peer {
    Process(Mutex<Child>),
    /// Tests run the worker on a thread, as no ferge binary is at hand.
    #[cfg(test)]
    Thread,
}

impl Worker {
    /// Starts a worker.
    pub(crate) fn start() -> Result<Self, DocumentError> {
        let Some(program) = PROGRAM.get() else {
            #[cfg(test)]
            return Ok(Self::on_thread());
            #[cfg(not(test))]
            {
                tracing::error!("no preview worker is set up");
                return Err(DocumentError::Damaged);
            }
        };
        let mut child = Command::new(program)
            .arg(ARGUMENT)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                tracing::warn!(%error, "cannot start the preview worker");
                DocumentError::Damaged
            })?;
        let (Some(input), Some(output)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(DocumentError::Damaged);
        };
        Ok(Self {
            child: Peer::Process(Mutex::new(child)),
            input: Box::new(input),
            output: Box::new(output),
        })
    }

    #[cfg(test)]
    fn on_thread() -> Self {
        let (request_reader, request_writer) = io::pipe().expect("pipe");
        let (reply_reader, reply_writer) = io::pipe().expect("pipe");
        std::thread::spawn(move || {
            let mut input = request_reader;
            let mut output = reply_writer;
            let _ = serve(&mut input, &mut output);
        });
        Self {
            child: Peer::Thread,
            input: Box::new(request_writer),
            output: Box::new(reply_reader),
        }
    }

    /// Sends `request` and waits for the reply, at most [`TIMEOUT`]. A
    /// worker that does not answer in time is killed.
    pub(crate) fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, DocumentError> {
        let (done, finished) = mpsc::channel::<()>();
        let child = &self.child;
        let input = &mut self.input;
        let output = &mut self.output;
        std::thread::scope(|scope| {
            scope.spawn(move || {
                if finished.recv_timeout(TIMEOUT) == Err(mpsc::RecvTimeoutError::Timeout) {
                    tracing::debug!("the preview worker took too long");
                    child.kill();
                }
            });
            let result = write_frame(input, request).and_then(|()| read_frame(output));
            let _ = done.send(());
            result.map_err(|_| DocumentError::TooLarge)
        })
    }

    /// Sends `request` and reads the status of the reply; the body follows
    /// in the returned bytes.
    pub(crate) fn call(&mut self, request: &[u8]) -> Result<Vec<u8>, DocumentError> {
        let mut reply = self.exchange(request)?;
        let status = reply.first().copied().ok_or(DocumentError::Damaged)?;
        match status {
            OK => {
                reply.remove(0);
                Ok(reply)
            }
            ENCRYPTED => Err(DocumentError::Encrypted),
            TOO_LARGE => Err(DocumentError::TooLarge),
            _ => Err(DocumentError::Damaged),
        }
    }
}

impl Peer {
    fn kill(&self) {
        match self {
            Self::Process(child) => {
                if let Ok(mut child) = child.lock() {
                    let _ = child.kill();
                }
            }
            #[cfg(test)]
            Self::Thread => {}
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Peer::Process(child) = &self.child
            && let Ok(mut child) = child.lock()
        {
            // A worker that is done exits by itself once stdin closes; one
            // still busy is stopped. Either way it is reaped.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The sheets of a workbook, read by a worker.
pub fn workbook(bytes: &[u8], format: WorkbookFormat) -> Result<Vec<Sheet>, DocumentError> {
    let mut request = Writer::new();
    request.u8(WORKBOOK);
    request.u8(match format {
        WorkbookFormat::Xlsx => 0,
        WorkbookFormat::Xlsb => 1,
        WorkbookFormat::Ods => 2,
    });
    request.bytes(bytes);
    let reply = Worker::start()?.call(&request.finish())?;
    decode_sheets(&reply)
}

/// The text of a document, read by a worker.
pub fn document(bytes: &[u8], format: TextFormat) -> Result<Document, DocumentError> {
    let mut request = Writer::new();
    request.u8(TEXT);
    request.u8(match format {
        TextFormat::Docx => 0,
        TextFormat::Pptx => 1,
        TextFormat::Odt => 2,
        TextFormat::Odp => 3,
    });
    request.bytes(bytes);
    let reply = Worker::start()?.call(&request.finish())?;
    decode_document(&reply)
}

/// A picture, decoded by a worker (see [`picture::decode`]).
pub fn picture(bytes: &[u8], svg: bool) -> Result<Picture, DocumentError> {
    let mut request = Writer::new();
    request.u8(PICTURE);
    request.bool(svg);
    request.bytes(bytes);
    let reply = Worker::start()?.call(&request.finish())?;
    decode_picture(&reply)
}

/// The request that opens a PDF in a worker.
pub(crate) fn pdf_open_request(bytes: &[u8]) -> Vec<u8> {
    let mut request = Writer::new();
    request.u8(PDF_OPEN);
    request.bytes(bytes);
    request.finish()
}

/// The request that renders a page of the open PDF.
pub(crate) fn pdf_render_request(page: usize, width: u32) -> Vec<u8> {
    let mut request = Writer::new();
    request.u8(PDF_RENDER);
    request.len(page);
    request.u32(width);
    request.finish()
}

/// Reads a page request in the worker: the page and the width.
pub(crate) fn read_render_request(frame: &[u8]) -> Result<(usize, u32), DocumentError> {
    let mut reader = Reader::new(frame);
    if reader.u8()? != PDF_RENDER {
        return Err(DocumentError::Damaged);
    }
    let page = reader.len()?;
    let width = reader.u32()?;
    reader.end()?;
    Ok((page, width))
}

// ---------------------------------------------------------------------------
// Encoding of results

fn workbook_format(tag: u8) -> Result<WorkbookFormat, DocumentError> {
    match tag {
        0 => Ok(WorkbookFormat::Xlsx),
        1 => Ok(WorkbookFormat::Xlsb),
        2 => Ok(WorkbookFormat::Ods),
        _ => Err(DocumentError::Damaged),
    }
}

fn text_format(tag: u8) -> Result<TextFormat, DocumentError> {
    match tag {
        0 => Ok(TextFormat::Docx),
        1 => Ok(TextFormat::Pptx),
        2 => Ok(TextFormat::Odt),
        3 => Ok(TextFormat::Odp),
        _ => Err(DocumentError::Damaged),
    }
}

fn encode_rows(writer: &mut Writer, rows: &[Vec<String>]) {
    writer.len(rows.len());
    for row in rows {
        writer.len(row.len());
        for cell in row {
            writer.str(cell);
        }
    }
}

fn decode_rows(reader: &mut Reader<'_>) -> Result<Vec<Vec<String>>, DocumentError> {
    let count = reader.count(4)?;
    let mut rows = Vec::with_capacity(count);
    for _ in 0..count {
        let cells = reader.count(4)?;
        let mut row = Vec::with_capacity(cells);
        for _ in 0..cells {
            row.push(reader.string()?);
        }
        rows.push(row);
    }
    Ok(rows)
}

fn encode_sheets(sheets: &[Sheet]) -> Writer {
    let mut writer = Writer::new();
    writer.len(sheets.len());
    for sheet in sheets {
        writer.str(&sheet.name);
        writer.bool(sheet.truncated);
        encode_rows(&mut writer, &sheet.rows);
    }
    writer
}

fn decode_sheets(bytes: &[u8]) -> Result<Vec<Sheet>, DocumentError> {
    let mut reader = Reader::new(bytes);
    let count = reader.count(9)?;
    let mut sheets = Vec::with_capacity(count);
    for _ in 0..count {
        let name = reader.string()?;
        let truncated = reader.bool()?;
        let rows = decode_rows(&mut reader)?;
        sheets.push(Sheet {
            name,
            rows,
            truncated,
        });
    }
    reader.end()?;
    Ok(sheets)
}

fn encode_document(document: &Document) -> Writer {
    let mut writer = Writer::new();
    writer.bool(document.truncated);
    writer.len(document.blocks.len());
    for block in &document.blocks {
        match block {
            Block::Heading { level, text } => {
                writer.u8(0);
                writer.u8(*level);
                writer.str(text);
            }
            Block::Paragraph(text) => {
                writer.u8(1);
                writer.str(text);
            }
            Block::Item { depth, text } => {
                writer.u8(2);
                writer.u8(*depth);
                writer.str(text);
            }
            Block::Table(rows) => {
                writer.u8(3);
                encode_rows(&mut writer, rows);
            }
            Block::Slide(number) => {
                writer.u8(4);
                writer.len(*number);
            }
        }
    }
    writer
}

fn decode_document(bytes: &[u8]) -> Result<Document, DocumentError> {
    let mut reader = Reader::new(bytes);
    let truncated = reader.bool()?;
    let count = reader.count(5)?;
    let mut blocks = Vec::with_capacity(count);
    for _ in 0..count {
        blocks.push(match reader.u8()? {
            0 => Block::Heading {
                level: reader.u8()?,
                text: reader.string()?,
            },
            1 => Block::Paragraph(reader.string()?),
            2 => Block::Item {
                depth: reader.u8()?,
                text: reader.string()?,
            },
            3 => Block::Table(decode_rows(&mut reader)?),
            4 => Block::Slide(reader.len()?),
            _ => return Err(DocumentError::Damaged),
        });
    }
    reader.end()?;
    Ok(Document { blocks, truncated })
}

pub(crate) fn encode_bitmap(writer: &mut Writer, bitmap: &Bitmap) {
    writer.u32(bitmap.width);
    writer.u32(bitmap.height);
    writer.bytes(&bitmap.rgba);
}

pub(crate) fn decode_bitmap(reader: &mut Reader<'_>) -> Result<Bitmap, DocumentError> {
    let width = reader.u32()?;
    let height = reader.u32()?;
    let rgba = reader.bytes()?;
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4));
    if width == 0 || height == 0 || expected != Some(rgba.len()) {
        return Err(DocumentError::Damaged);
    }
    Ok(Bitmap {
        width,
        height,
        rgba: rgba.to_vec(),
    })
}

fn encode_picture(picture: &Picture) -> Writer {
    let mut writer = Writer::new();
    writer.len(picture.frames.len());
    for frame in &picture.frames {
        writer.u32(frame.delay_ms);
        encode_bitmap(&mut writer, &frame.bitmap);
    }
    writer
}

fn decode_picture(bytes: &[u8]) -> Result<Picture, DocumentError> {
    let mut reader = Reader::new(bytes);
    let count = reader.count(16)?;
    if count == 0 {
        return Err(DocumentError::Damaged);
    }
    let mut frames = Vec::with_capacity(count);
    for _ in 0..count {
        let delay_ms = reader.u32()?;
        let bitmap = decode_bitmap(&mut reader)?;
        frames.push(Frame { bitmap, delay_ms });
    }
    reader.end()?;
    Ok(Picture { frames })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sheets_and_documents_round_trip() {
        let sheets = vec![
            Sheet {
                name: "Eins".into(),
                rows: vec![vec!["a".into(), String::new(), "c".into()], vec![]],
                truncated: true,
            },
            Sheet::default(),
        ];
        assert_eq!(decode_sheets(&encode_sheets(&sheets).finish()), Ok(sheets));

        let document = Document {
            blocks: vec![
                Block::Slide(3),
                Block::Heading {
                    level: 2,
                    text: "Titel".into(),
                },
                Block::Paragraph("Text".into()),
                Block::Item {
                    depth: 1,
                    text: "Punkt".into(),
                },
                Block::Table(vec![vec!["x".into(), "y".into()]]),
            ],
            truncated: false,
        };
        assert_eq!(
            decode_document(&encode_document(&document).finish()),
            Ok(document)
        );
    }

    #[test]
    fn bitmaps_must_match_their_size() {
        let mut writer = Writer::new();
        encode_bitmap(
            &mut writer,
            &Bitmap {
                width: 2,
                height: 2,
                rgba: vec![0; 15],
            },
        );
        let bytes = writer.finish();
        assert_eq!(
            decode_bitmap(&mut Reader::new(&bytes)),
            Err(DocumentError::Damaged)
        );
    }

    #[test]
    fn workers_answer_and_report_damage() {
        assert_eq!(
            workbook(b"not a workbook", WorkbookFormat::Xlsx),
            Err(DocumentError::Damaged)
        );
        assert_eq!(
            document(b"not a document", TextFormat::Docx),
            Err(DocumentError::Damaged)
        );
        let mut worker = Worker::start().unwrap();
        assert_eq!(worker.call(&[99]), Err(DocumentError::Damaged));
    }
}
