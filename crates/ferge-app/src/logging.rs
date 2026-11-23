//! Logging: to standard error as before, and to a file for debugging.
//!
//! The file is `ferge.log` in the state directory (`~/.local/state/ferge` on
//! Linux). Its level comes from `FERGE_LOG` (default: `info`, and `debug` for
//! ferge-core), independent of `RUST_LOG` for standard error. A log above
//! 20 MB is moved to `ferge.log.1` at startup. Lines go to a background
//! thread through a bounded queue: logging never blocks, and under extreme
//! load lines are dropped rather than slowing the UI down.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::mpsc;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;

/// Lines waiting for the writer thread.
const QUEUE: usize = 4096;

/// Logged at error level by the certificate verifier whenever the system
/// does not trust a certificate, which ferge then asks about: an expected
/// case, not an error. ferge logs what matters about it itself.
const QUIET: &str = "rustls_platform_verifier=off";

/// Size at which the log is rotated at startup.
const ROTATE_BYTES: u64 = 20 * 1024 * 1024;

/// The folder for logs and transfer traces; `None` without a home.
pub(crate) fn state_dir() -> Option<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|dir| dir.join("ferge"))
}

/// Sets up logging. `to_file` is off for benchmarks, which must not touch
/// the user's files.
pub(crate) fn init(to_file: bool) {
    let stderr = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| format!("info,{QUIET}").into()),
        );
    let file = to_file.then(state_dir).flatten().map(|dir| {
        let filter = EnvFilter::try_from_env("FERGE_LOG")
            .unwrap_or_else(|_| format!("info,ferge_core=debug,russh=warn,{QUIET}").into());
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_thread_names(true)
            .with_writer(LogFile::start(dir.join("ferge.log")))
            .with_filter(filter)
    });
    tracing_subscriber::registry()
        .with(stderr)
        .with(file)
        .init();
    if let Some(dir) = to_file.then(state_dir).flatten() {
        tracing::info!(log = %dir.join("ferge.log").display(), "logging to a file");
    }
}

/// Hands formatted lines to the writer thread.
#[derive(Clone, Debug)]
struct LogFile {
    sender: mpsc::SyncSender<Vec<u8>>,
}

impl LogFile {
    fn start(path: PathBuf) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(QUEUE);
        let spawned = std::thread::Builder::new()
            .name("ferge-log".into())
            .spawn(move || write_lines(&path, &receiver));
        if let Err(error) = spawned {
            eprintln!("ferge: no log file: {error}");
        }
        Self { sender }
    }
}

fn write_lines(path: &std::path::Path, receiver: &mpsc::Receiver<Vec<u8>>) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > ROTATE_BYTES) {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) => {
            eprintln!("ferge: cannot open {}: {error}", path.display());
            return;
        }
    };
    let mut writer = std::io::BufWriter::new(file);
    while let Ok(line) = receiver.recv() {
        if writer.write_all(&line).is_err() {
            return;
        }
        // Flush whenever the queue is drained, so a crash loses little.
        let mut more = false;
        while let Ok(line) = receiver.try_recv() {
            more = true;
            if writer.write_all(&line).is_err() {
                return;
            }
        }
        if !more || writer.buffer().len() > 64 * 1024 {
            let _ = writer.flush();
        }
    }
}

/// One formatted event; sent when dropped.
struct LogLine {
    buffer: Vec<u8>,
    sender: mpsc::SyncSender<Vec<u8>>,
}

impl std::io::Write for LogLine {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for LogLine {
    fn drop(&mut self) {
        if !self.buffer.is_empty() {
            let _ = self.sender.try_send(std::mem::take(&mut self.buffer));
        }
    }
}

impl<'a> MakeWriter<'a> for LogFile {
    type Writer = LogLine;

    fn make_writer(&'a self) -> Self::Writer {
        LogLine {
            buffer: Vec::with_capacity(256),
            sender: self.sender.clone(),
        }
    }
}
