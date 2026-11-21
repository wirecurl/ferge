//! Quick Look on macOS: the system renders a document as its program
//! would, and ferge shows the picture.
//!
//! Rendering runs in `qlmanage`, which ships with macOS, in a process of its
//! own: a Quick Look generator that hangs or crashes cannot take ferge with
//! it, and no Objective-C is needed (the workspace forbids `unsafe`).
//! Elsewhere [`render`] fails, and callers fall back to their own readers.

use std::path::Path;
use std::time::Duration;

use bytes::Bytes;

/// How long Quick Look may take for one file.
const TIMEOUT: Duration = Duration::from_secs(15);

/// Why Quick Look gave no picture.
#[derive(Debug, thiserror::Error)]
pub enum QuickLookError {
    #[error("Quick Look is not available here")]
    Unavailable(#[source] std::io::Error),
    #[error("Quick Look has no picture of this file")]
    NoPicture,
    #[error("Quick Look took too long")]
    TimedOut,
    #[error("cannot prepare the file for Quick Look")]
    Io(#[source] std::io::Error),
}

/// A PNG of the first page of `file`, at most `size` pixels on its longer
/// side.
pub async fn render(file: &Path, size: u32) -> Result<Bytes, QuickLookError> {
    let output = tempfile::tempdir().map_err(QuickLookError::Io)?;
    let mut command = tokio::process::Command::new("/usr/bin/qlmanage");
    command
        .arg("-t")
        .arg("-s")
        .arg(size.to_string())
        .arg("-o")
        .arg(output.path())
        .arg(file)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let status = tokio::time::timeout(TIMEOUT, command.status())
        .await
        .map_err(|_| QuickLookError::TimedOut)?
        .map_err(QuickLookError::Unavailable)?;
    if !status.success() {
        return Err(QuickLookError::NoPicture);
    }
    // qlmanage names the picture after the file, with `.png` added.
    let name = file.file_name().ok_or(QuickLookError::NoPicture)?;
    let mut picture = name.to_os_string();
    picture.push(".png");
    match tokio::fs::read(output.path().join(picture)).await {
        Ok(png) => Ok(png.into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(QuickLookError::NoPicture)
        }
        Err(error) => Err(QuickLookError::Io(error)),
    }
}

/// Like [`render`], for a file held in memory, such as one read from a
/// server. It is written to a private temporary folder under `name`, whose
/// extension tells Quick Look what it is, and removed afterwards.
pub async fn render_bytes(name: &str, bytes: Bytes, size: u32) -> Result<Bytes, QuickLookError> {
    let folder = tempfile::tempdir().map_err(QuickLookError::Io)?;
    // Only the last part of the name, so it cannot point elsewhere.
    let name = Path::new(name)
        .file_name()
        .ok_or(QuickLookError::NoPicture)?;
    let file = folder.path().join(name);
    tokio::fs::write(&file, &bytes)
        .await
        .map_err(QuickLookError::Io)?;
    render(&file, size).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn is_unavailable_off_macos() {
        let result = render_bytes("a.docx", Bytes::from_static(b"x"), 256).await;
        assert!(matches!(result, Err(QuickLookError::Unavailable(_))));
    }

    #[tokio::test]
    async fn keeps_names_inside_the_temporary_folder() {
        let result = render_bytes("..", Bytes::from_static(b"x"), 256).await;
        assert!(matches!(result, Err(QuickLookError::NoPicture)));
    }

    /// Run on a Mac: `cargo test -p ferge-core quick_look`.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn renders_a_text_document() {
        let rtf = br"{\rtf1\ansi{\fonttbl\f0 Helvetica;}\f0\fs48 Hello from ferge}";
        let png = render_bytes("hello.rtf", Bytes::from_static(rtf), 512)
            .await
            .unwrap();
        assert!(png.starts_with(b"\x89PNG"));
    }
}
