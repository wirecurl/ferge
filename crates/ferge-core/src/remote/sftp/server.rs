//! [`ServerSide`] for SFTP sessions: copying on the server with the
//! `copy-data` extension or `cp`, and running commands.

use std::time::Duration;

use async_trait::async_trait;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::{FileAttributes, OpenFlags, Packet, StatusCode};

use super::{SftpRemote, sftp_error};
use crate::error::{Error, Result};
use crate::path::RemotePath;
use crate::remote::server::{CommandOutput, CopyMethod, CopyProgress, Endpoint, ServerSide};
use crate::remote::shell::octal_escape;

/// OpenSSH extension (9.0 and later) that copies between two open handles.
pub(super) const COPY_DATA: &str = "copy-data";

/// Bytes per `copy-data` request. Every request has to finish within the
/// SFTP request timeout, and smaller requests report progress more often.
const COPY_CHUNK: u64 = 32 * 1024 * 1024;

/// How long `cp` may take for one file.
const CP_TIMEOUT: Duration = Duration::from_hours(24);

/// Copies the two paths read from standard input, one per line, octal-escaped
/// (see [`octal_escape`]). Single-quoted without `!` or backslashes, so every
/// login shell hands it to `sh` unchanged.
const CP_COMMAND: &str = "sh -c 'IFS= read -r a && IFS= read -r b && a=$(printf \"%bx\" \"$a\") \
                          && b=$(printf \"%bx\" \"$b\") && exec cp \"${a%x}\" \"${b%x}\"'";

#[async_trait]
impl ServerSide for SftpRemote {
    fn endpoint(&self) -> &Endpoint {
        &self.inner.endpoint
    }

    fn runs_commands(&self) -> bool {
        !self.inner.elevated
    }

    async fn run(
        &self,
        command: &str,
        input: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<CommandOutput> {
        super::exec::run(&self.inner.ssh, command, input, timeout)
            .await?
            .ok_or(Error::Unsupported(
                "the server does not run commands for this account",
            ))
    }

    async fn copy_within(
        &self,
        from: &RemotePath,
        to: &RemotePath,
        offset: u64,
        progress: CopyProgress<'_>,
    ) -> Result<(u64, CopyMethod)> {
        if self.inner.copy_data {
            let length = self.copy_data(from, to, offset, progress).await?;
            return Ok((length, CopyMethod::CopyData));
        }
        if !self.runs_commands() {
            return Err(Error::Unsupported("copying on the server"));
        }
        // cp cannot continue a partial file; start over.
        progress(0);
        let input = format!(
            "{}\n{}\n",
            octal_escape(from.as_str()),
            octal_escape(to.as_str())
        );
        let output = self
            .run(CP_COMMAND, Some(input.as_bytes()), CP_TIMEOUT)
            .await?;
        if !output.success() {
            let message = output.stderr_text().trim().to_owned();
            return Err(Error::Remote {
                path: from.clone(),
                message: if message.is_empty() {
                    format!("cp failed with status {:?}", output.status)
                } else {
                    message
                },
            });
        }
        let length = crate::Remote::stat(self, to).await?.size;
        progress(length);
        Ok((length, CopyMethod::Cp))
    }
}

impl SftpRemote {
    /// Copies with `copy-data` on a data channel, so browsing on the
    /// metadata channel goes on meanwhile.
    async fn copy_data(
        &self,
        from: &RemotePath,
        to: &RemotePath,
        offset: u64,
        progress: CopyProgress<'_>,
    ) -> Result<u64> {
        let channel = self.data().await?;
        let session = &channel.session;
        let source = session
            .open(from.as_str(), OpenFlags::READ, FileAttributes::empty())
            .await
            .map_err(|error| sftp_error(from, error))?
            .handle;
        let mut flags = OpenFlags::WRITE | OpenFlags::CREATE;
        if offset == 0 {
            flags |= OpenFlags::TRUNCATE;
        }
        let target = match session
            .open(to.as_str(), flags, FileAttributes::empty())
            .await
        {
            Ok(handle) => handle.handle,
            Err(error) => {
                let _ = session.close(source).await;
                return Err(sftp_error(to, error));
            }
        };
        let copied = async {
            if offset > 0 {
                let attrs = FileAttributes {
                    size: Some(offset),
                    ..FileAttributes::empty()
                };
                session
                    .fsetstat(target.as_str(), attrs)
                    .await
                    .map_err(|error| sftp_error(to, error))?;
            }
            let size = session
                .fstat(source.as_str())
                .await
                .map_err(|error| sftp_error(from, error))?
                .attrs
                .size
                .unwrap_or(0);
            let mut at = offset;
            loop {
                // The last request has no length: it copies up to the end of
                // the file, even if it grew in the meantime.
                let length = if size.saturating_sub(at) > COPY_CHUNK {
                    COPY_CHUNK
                } else {
                    0
                };
                let request = copy_request(&source, at, length, &target)?;
                check_status(session.extended(COPY_DATA, request).await, from)?;
                if length == 0 {
                    break;
                }
                at += length;
                progress(at);
            }
            let length = session
                .fstat(target.as_str())
                .await
                .map_err(|error| sftp_error(to, error))?
                .attrs
                .size
                .unwrap_or(at);
            progress(length);
            Ok::<_, Error>(length)
        }
        .await;
        let closed = session.close(target).await;
        let _ = session.close(source).await;
        let length = copied?;
        closed.map_err(|error| sftp_error(to, error))?;
        Ok(length)
    }
}

/// The body of a `copy-data` request: read handle, read offset, length (0
/// for "to the end"), write handle, write offset.
fn copy_request(source: &str, at: u64, length: u64, target: &str) -> Result<Vec<u8>> {
    let mut data = Vec::with_capacity(source.len() + target.len() + 32);
    let string = |data: &mut Vec<u8>, text: &str| -> Result<()> {
        let len = u32::try_from(text.len())
            .map_err(|_| Error::protocol("copying on the server failed", "handle too long"))?;
        data.extend_from_slice(&len.to_be_bytes());
        data.extend_from_slice(text.as_bytes());
        Ok(())
    };
    string(&mut data, source)?;
    data.extend_from_slice(&at.to_be_bytes());
    data.extend_from_slice(&length.to_be_bytes());
    string(&mut data, target)?;
    data.extend_from_slice(&at.to_be_bytes());
    Ok(data)
}

fn check_status(reply: std::result::Result<Packet, SftpError>, path: &RemotePath) -> Result<()> {
    match reply {
        Ok(Packet::Status(status)) if status.status_code == StatusCode::Ok => Ok(()),
        Ok(Packet::Status(status)) => Err(sftp_error(path, SftpError::Status(status))),
        Ok(_) => Err(Error::protocol(
            "copying on the server failed",
            "unexpected reply",
        )),
        Err(error) => Err(sftp_error(path, error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_copy_requests() {
        let data = copy_request("ab", 1, 2, "c").unwrap();
        let mut expected = vec![0, 0, 0, 2, b'a', b'b'];
        expected.extend_from_slice(&1u64.to_be_bytes());
        expected.extend_from_slice(&2u64.to_be_bytes());
        expected.extend_from_slice(&[0, 0, 0, 1, b'c']);
        expected.extend_from_slice(&1u64.to_be_bytes());
        assert_eq!(data, expected);
    }

    #[test]
    fn cp_command_suits_every_login_shell() {
        assert!(!CP_COMMAND.contains('\n'));
        assert!(!CP_COMMAND.contains('!'));
        assert!(!CP_COMMAND.contains('\\'));
        assert_eq!(CP_COMMAND.matches('\'').count(), 2);
    }
}
