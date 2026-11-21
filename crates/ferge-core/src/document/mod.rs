//! Reading documents for previews: spreadsheets ([`sheet`]), word processor
//! and presentation files as text ([`text`]) and PDF pages ([`pdf`]).
//!
//! Everything here works on bytes already read and is CPU work; callers run
//! it off the UI thread. The files may come from anywhere, so every reader is
//! bounded: archives are checked before they are unpacked, and rows, columns,
//! blocks and text stop at fixed limits instead of growing with the input.

mod archive;
pub mod pdf;
pub mod sheet;
pub mod text;
mod xml;

/// Why a document cannot be shown.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DocumentError {
    #[error("This file is damaged, or it is not what its name says.")]
    Damaged,
    #[error("This document is protected by a password.")]
    Encrypted,
    #[error("This document is too large to preview.")]
    TooLarge,
}

/// Whether `bytes` start like an OLE compound file: the container of old
/// Office formats and of password-protected new ones.
fn is_compound_file(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1])
}

/// Shortens `text` to at most `limit` characters, marking the cut with `…`.
fn shorten(mut text: String, limit: usize) -> String {
    if let Some((end, _)) = text.char_indices().nth(limit) {
        text.truncate(end);
        text.push('…');
    }
    text
}
