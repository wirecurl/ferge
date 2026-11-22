//! Running a command on the server over its own SSH channel.

use std::time::{Duration, Instant};

use russh::ChannelMsg;
use russh::client;

use super::handler::ClientHandler;
use crate::error::{Error, Result};
use crate::remote::server::CommandOutput;

/// Runs `command` through the login shell, feeds it `input` and collects what
/// it prints. Returns `None` if the server refuses to run commands for this
/// account (`ForceCommand`, SFTP-only accounts).
pub(super) async fn run(
    ssh: &client::Handle<ClientHandler>,
    command: &str,
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<Option<CommandOutput>> {
    let started = Instant::now();
    let work = async {
        let mut channel = ssh
            .channel_open_session()
            .await
            .map_err(|error| Error::protocol("opening an SSH channel failed", error))?;
        channel
            .exec(true, command)
            .await
            .map_err(|error| Error::protocol("running a command failed", error))?;
        if let Some(input) = input {
            channel
                .data(input)
                .await
                .map_err(|error| Error::protocol("running a command failed", error))?;
        }
        channel
            .eof()
            .await
            .map_err(|error| Error::protocol("running a command failed", error))?;
        let mut output = CommandOutput::default();
        while let Some(message) = channel.wait().await {
            match message {
                ChannelMsg::Failure => return Ok(None),
                ChannelMsg::Data { data } => keep(&mut output.stdout, &data),
                ChannelMsg::ExtendedData { data, ext: 1 } => keep(&mut output.stderr, &data),
                ChannelMsg::ExitStatus { exit_status } => output.status = Some(exit_status),
                _ => {}
            }
        }
        Ok(Some(output))
    };
    let result = tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| Error::Timeout("running a command on the server"))?;
    match &result {
        Ok(Some(output)) => tracing::debug!(
            command = %abbreviate(command),
            status = ?output.status,
            stdout = output.stdout.len(),
            stderr = %abbreviate(&output.stderr_text()),
            elapsed_ms = started.elapsed().as_millis(),
            "ran a command"
        ),
        Ok(None) => tracing::debug!(command = %abbreviate(command), "the server refused a command"),
        Err(error) => tracing::debug!(command = %abbreviate(command), %error, "a command failed"),
    }
    result
}

/// Bytes kept of what a command prints on each stream; a server that
/// prints without end cannot fill ferge's memory.
const OUTPUT_LIMIT: usize = 16 * 1024 * 1024;

/// Appends `data` to `buffer` up to [`OUTPUT_LIMIT`] bytes.
fn keep(buffer: &mut Vec<u8>, data: &[u8]) {
    let room = OUTPUT_LIMIT.saturating_sub(buffer.len());
    buffer.extend_from_slice(&data[..data.len().min(room)]);
}

/// The first line of `text`, cut to a length that fits a log line.
fn abbreviate(text: &str) -> String {
    const MAX: usize = 300;
    let line = text.trim().lines().next().unwrap_or_default();
    match line.char_indices().nth(MAX) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None if line.len() < text.trim().len() => format!("{line} …"),
        None => line.to_owned(),
    }
}
