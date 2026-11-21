//! Working as root: a second set of SFTP channels on the same SSH connection,
//! served by `sudo sftp-server` instead of the login's SFTP subsystem.
//!
//! Nothing has to be set up on the server or typed in by the user. One probe
//! finds the `sftp-server` program and whether sudo needs a password. A
//! password is checked with a throwaway `sudo true` first: sudo reads it from
//! standard input, and on the SFTP channels a wrong one would make sudo read
//! the SFTP handshake as further attempts. Every channel then starts its own
//! `sudo -S sftp-server` with the password as the first line of its input;
//! sudo reads up to the newline and leaves the rest to sftp-server. All
//! channels open at once, so the password is not kept afterwards.

use std::time::Duration;

use russh::ChannelMsg;
use russh::client;
use secrecy::{ExposeSecret as _, SecretString};
use zeroize::Zeroizing;

use super::handler::ClientHandler;
use super::{REQUEST_TIMEOUT_SECS, SftpChannel};
use crate::error::{ElevationError, Error, Result};

/// Where `sftp-server` lives on common systems, tried after the path in
/// `sshd_config`: Debian and Ubuntu, Fedora and RHEL, Arch and Alpine, macOS
/// and the BSDs, SUSE, and installations below `/usr/local`.
const SFTP_SERVERS: &[&str] = &[
    "/usr/lib/openssh/sftp-server",
    "/usr/libexec/openssh/sftp-server",
    "/usr/lib/ssh/sftp-server",
    "/usr/libexec/sftp-server",
    "/usr/lib/sftp-server",
    "/usr/local/libexec/sftp-server",
    "/usr/local/lib/sftp-server",
];

/// Runs sudo with messages in English, so its replies can be told apart.
const SUDO: &str = "env LC_ALL=C sudo";

/// How sudo gets to run a program as root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SudoMode {
    /// Rules allow it without a password (`NOPASSWD`).
    NoPassword,
    /// sudo asks for a password.
    Password,
}

/// What the probe found out about the server.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Probe {
    /// The first executable `sftp-server`.
    pub server: Option<String>,
    /// `None` when sudo is not installed.
    pub sudo: Option<SudoMode>,
}

/// The command a channel runs for an SFTP session as root.
pub(super) struct ElevatedCommand {
    command: String,
    password: Option<SecretString>,
}

/// One shell command line that probes the server. It has to work in any login
/// shell (sh, bash, zsh, fish, csh), so it hands a single-quoted script without
/// `!`, backslashes or line breaks to `sh`.
fn probe_command() -> String {
    let candidates = SFTP_SERVERS.join(" ");
    format!(
        "sh -c '\
         for p in $(grep -i \"^[[:space:]]*subsystem[[:space:]][[:space:]]*sftp\" /etc/ssh/sshd_config 2>/dev/null) {candidates}; do \
         case $p in /*) if [ -x \"$p\" ]; then echo \"server $p\"; fi;; esac; \
         done; \
         if command -v sudo >/dev/null 2>&1; then \
         if sudo -n -k true >/dev/null 2>&1; then echo \"sudo nopasswd\"; else echo \"sudo password\"; fi; \
         else echo \"sudo missing\"; fi'"
    )
}

/// Reads the probe's output.
pub(super) fn parse_probe(output: &str) -> Probe {
    let mut probe = Probe {
        server: None,
        sudo: None,
    };
    for line in output.lines() {
        if let Some(path) = line.strip_prefix("server ") {
            if probe.server.is_none() && is_plain_path(path) {
                probe.server = Some(path.to_owned());
            }
        } else if let Some(mode) = line.strip_prefix("sudo ") {
            probe.sudo = match mode {
                "nopasswd" => Some(SudoMode::NoPassword),
                "password" => Some(SudoMode::Password),
                _ => None,
            };
        }
    }
    probe
}

/// Whether `path` can go into a command line unquoted: absolute, and only
/// characters that no shell treats specially.
fn is_plain_path(path: &str) -> bool {
    path.starts_with('/')
        && path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/._+-".contains(&byte))
}

/// What sudo's reply to a password means.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum SudoReply {
    Accepted,
    WrongPassword,
    /// Anything else, with the line that explains it.
    Refused(String),
}

/// Interprets the exit status and standard error of `sudo -S … true`.
pub(super) fn sudo_reply(status: Option<u32>, stderr: &str) -> SudoReply {
    if status == Some(0) {
        return SudoReply::Accepted;
    }
    if stderr.contains("incorrect password") || stderr.contains("Sorry, try again") {
        return SudoReply::WrongPassword;
    }
    // The first time, sudo prints its lecture before anything else.
    let lecture = |line: &str| {
        line.starts_with(char::is_whitespace)
            || line.contains("usual lecture")
            || line.starts_with("Administrator.")
            || line.contains("will not be visible")
            || line.contains("incident has been reported")
    };
    let message = stderr
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty() && !lecture(line))
        .map(|line| line.strip_prefix("sudo: ").unwrap_or(line))
        .next()
        .unwrap_or("it did not say why");
    SudoReply::Refused(message.trim_end_matches('.').to_owned())
}

/// The outcome of a command run on the server.
struct Output {
    status: Option<u32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs `command`, feeds it `input` and collects what it prints.
async fn run(
    ssh: &client::Handle<ClientHandler>,
    host: &str,
    command: &str,
    input: Option<&[u8]>,
) -> Result<Output> {
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
        let mut output = Output {
            status: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        while let Some(message) = channel.wait().await {
            match message {
                ChannelMsg::Failure => {
                    return Err(ElevationError::CommandsRefused {
                        host: host.to_owned(),
                    }
                    .into());
                }
                ChannelMsg::Data { data } => output.stdout.extend_from_slice(&data),
                ChannelMsg::ExtendedData { data, ext: 1 } => {
                    output.stderr.extend_from_slice(&data);
                }
                ChannelMsg::ExitStatus { exit_status } => output.status = Some(exit_status),
                _ => {}
            }
        }
        Ok(output)
    };
    tokio::time::timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS), work)
        .await
        .map_err(|_| Error::Timeout("running a command on the server"))?
}

/// Finds out how to run `sftp-server` as root and asks for the password if
/// sudo needs one. `ask(retry)` asks the user; `None` cancels.
pub(super) async fn prepare<F, Fut>(
    ssh: &client::Handle<ClientHandler>,
    host: &str,
    mut ask: F,
) -> Result<ElevatedCommand>
where
    F: FnMut(bool) -> Fut,
    Fut: Future<Output = Option<SecretString>>,
{
    let output = run(ssh, host, &probe_command(), None).await?;
    let probe = parse_probe(&String::from_utf8_lossy(&output.stdout));
    tracing::debug!(?probe, "probed the server for working as root");
    let mode = probe.sudo.ok_or_else(|| ElevationError::NoSudo {
        host: host.to_owned(),
    })?;
    let server = probe.server.ok_or_else(|| ElevationError::NoSftpServer {
        host: host.to_owned(),
    })?;
    if mode == SudoMode::NoPassword {
        return Ok(ElevatedCommand {
            command: format!("{SUDO} -n -- {server}"),
            password: None,
        });
    }
    let mut retry = false;
    loop {
        let password = ask(retry).await.ok_or(ElevationError::Cancelled)?;
        let check = format!("{SUDO} -S -k -p \"\" -- true");
        let output = run(ssh, host, &check, Some(&password_line(&password))).await?;
        match sudo_reply(output.status, &String::from_utf8_lossy(&output.stderr)) {
            SudoReply::Accepted => {
                return Ok(ElevatedCommand {
                    // -k: ignore credentials sudo may have cached, so it always
                    // reads the password and never hands it to sftp-server.
                    command: format!("{SUDO} -S -k -p \"\" -- {server}"),
                    password: Some(password),
                });
            }
            SudoReply::WrongPassword => retry = true,
            SudoReply::Refused(message) => {
                return Err(ElevationError::Refused {
                    host: host.to_owned(),
                    message,
                }
                .into());
            }
        }
    }
}

fn password_line(password: &SecretString) -> Zeroizing<Vec<u8>> {
    let mut line = Zeroizing::new(Vec::with_capacity(password.expose_secret().len() + 1));
    line.extend_from_slice(password.expose_secret().as_bytes());
    line.push(b'\n');
    line
}

impl ElevatedCommand {
    /// Opens a channel running `sftp-server` as root, ready for the SFTP
    /// handshake.
    pub(super) async fn open(
        &self,
        ssh: &client::Handle<ClientHandler>,
        host: &str,
    ) -> Result<SftpChannel> {
        let not_started = |source: russh::Error| ElevationError::NotStarted {
            host: host.to_owned(),
            source: source.into(),
        };
        let mut channel = ssh
            .channel_open_session()
            .await
            .map_err(|error| Error::protocol("opening an SSH channel failed", error))?;
        channel
            .exec(true, self.command.as_str())
            .await
            .map_err(not_started)?;
        // Wait until the command runs, so its input reaches sudo.
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Success) => break,
                Some(ChannelMsg::Failure) => {
                    return Err(ElevationError::CommandsRefused {
                        host: host.to_owned(),
                    }
                    .into());
                }
                Some(ChannelMsg::ExitStatus { .. } | ChannelMsg::Eof | ChannelMsg::Close)
                | None => {
                    return Err(not_started(russh::Error::Disconnect).into());
                }
                Some(_) => {}
            }
        }
        if let Some(password) = &self.password {
            let line = password_line(password);
            channel.data(&line[..]).await.map_err(not_started)?;
        }
        Ok(channel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_prefers_the_first_server_and_reads_sudo() {
        let probe = parse_probe(
            "server /usr/lib/openssh/sftp-server\nserver /usr/lib/ssh/sftp-server\nsudo password\n",
        );
        assert_eq!(
            probe,
            Probe {
                server: Some("/usr/lib/openssh/sftp-server".into()),
                sudo: Some(SudoMode::Password),
            }
        );
        let probe = parse_probe("sudo nopasswd\n");
        assert_eq!(probe.server, None);
        assert_eq!(probe.sudo, Some(SudoMode::NoPassword));
        assert_eq!(parse_probe("sudo missing\n").sudo, None);
    }

    #[test]
    fn probe_skips_paths_that_would_need_quoting() {
        let probe = parse_probe(
            "server /opt/my server/sftp-server\nserver /bin/x;rm\nserver /usr/lib/ssh/sftp-server\n",
        );
        assert_eq!(probe.server.as_deref(), Some("/usr/lib/ssh/sftp-server"));
    }

    #[test]
    fn probe_command_is_one_line_for_every_shell() {
        let command = probe_command();
        assert!(!command.contains('\n'));
        assert!(!command.contains('!'));
        assert!(!command.contains('\\'));
        // The script itself is one single-quoted word.
        assert_eq!(command.matches('\'').count(), 2);
    }

    #[test]
    fn reads_sudo_replies() {
        assert_eq!(sudo_reply(Some(0), ""), SudoReply::Accepted);
        assert_eq!(
            sudo_reply(
                Some(1),
                "Sorry, try again.\nsudo: no password was provided\nsudo: 1 incorrect password attempt\n"
            ),
            SudoReply::WrongPassword
        );
        let lecture = "\nWe trust you have received the usual lecture from the local System\n\
                       Administrator. It usually boils down to these three things:\n\n\
                       \x20   #1) Respect the privacy of others.\n\n\
                       For security reasons, the password you type will not be visible.\n\n\
                       ferry-nosudo is not in the sudoers file.\n\
                       This incident has been reported to the administrator.\n";
        assert_eq!(
            sudo_reply(Some(1), lecture),
            SudoReply::Refused("ferry-nosudo is not in the sudoers file".into())
        );
        assert_eq!(
            sudo_reply(Some(1), "sudo: sorry, you must have a tty to run sudo\n"),
            SudoReply::Refused("sorry, you must have a tty to run sudo".into())
        );
    }
}
