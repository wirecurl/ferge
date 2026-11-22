//! Working as root: a second set of SFTP channels on the same SSH connection,
//! served by `sudo sftp-server` instead of the login's SFTP subsystem.
//!
//! Nothing has to be set up on the server or typed in by the user. A probe
//! finds the `sftp-server` program; a second one asks sudo, without a
//! password, whether running exactly that program would need one (sudoers
//! may allow it without a password while other commands need one). A
//! password is checked by running the same program once with no input
//! first: sudo reads the password from standard input, and on the SFTP
//! channels a wrong one would make sudo read the SFTP handshake as further
//! attempts. Every channel then starts its own `sudo -S sftp-server` with
//! the password as the first line of its input; sudo reads up to the
//! newline and leaves the rest to sftp-server. All channels open at once,
//! so the password is not kept afterwards.

use std::time::Duration;

use russh::ChannelMsg;
use russh::client;
use secrecy::{ExposeSecret as _, SecretString};
use zeroize::Zeroizing;

use super::handler::ClientHandler;
use super::{REQUEST_TIMEOUT_SECS, SftpChannel};
use crate::error::{ElevationError, Error, Result};
use crate::remote::server::CommandOutput;

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
/// Characters of a server's explanation shown in an error message.
const MESSAGE_LIMIT: usize = 200;
/// Passwords tried before giving up, as sudo itself does; saved passwords
/// that are tried first count too.
const PASSWORD_ATTEMPTS: usize = 3;

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
    /// Whether sudo is installed.
    pub sudo: bool,
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
         if command -v sudo >/dev/null 2>&1; then echo \"sudo present\"; else echo \"sudo missing\"; fi'"
    )
}

/// Reads the probe's output.
pub(super) fn parse_probe(output: &str) -> Probe {
    let mut probe = Probe {
        server: None,
        sudo: false,
    };
    for line in output.lines() {
        if let Some(path) = line.strip_prefix("server ") {
            if probe.server.is_none() && is_plain_path(path) {
                probe.server = Some(path.to_owned());
            }
        } else if line == "sudo present" {
            probe.sudo = true;
        }
    }
    probe
}

/// Whether `path` names an `sftp-server` program and can go into a command
/// line unquoted: absolute, and only characters that no shell treats
/// specially. Anything else a server's configuration names (a wrapper, a
/// shell) is not started as root.
fn is_plain_path(path: &str) -> bool {
    path.starts_with('/')
        && path.ends_with("/sftp-server")
        && path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/._+-".contains(&byte))
}

/// The command that asks sudo, without a password and without running
/// anything that reads input, whether `server` may run as root without one.
fn password_probe(server: &str) -> String {
    format!("{SUDO} -n -k -- {server} </dev/null")
}

/// Whether sudo's answer to [`password_probe`] means a password is needed.
fn needs_password(stderr: &str) -> bool {
    // sudo, and sudo-rs (the default on newer Ubuntu releases).
    [
        "password is required",
        "a terminal is required",
        "interactive authentication is required",
    ]
    .iter()
    .any(|wording| stderr.contains(wording))
}

/// A server's explanation as it may appear in ferge's messages: one line,
/// no control or invisible characters, at most [`MESSAGE_LIMIT`] characters.
fn quoted(message: &str) -> String {
    let mut out: String = message
        .chars()
        .filter(|c| !(c.is_control() || super::config::is_invisible_format_char(*c)))
        .take(MESSAGE_LIMIT + 1)
        .collect();
    if out.chars().count() > MESSAGE_LIMIT {
        out = out.chars().take(MESSAGE_LIMIT).collect();
        out.push('…');
    }
    format!("\u{201c}{out}\u{201d}")
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
        .map_or_else(
            || "it did not say why".to_owned(),
            |line| quoted(line.trim_end_matches('.')),
        );
    SudoReply::Refused(message)
}

/// Runs `command` and feeds it `input`; a refusal means the account may not
/// run commands at all.
async fn run(
    ssh: &client::Handle<ClientHandler>,
    host: &str,
    command: &str,
    input: Option<&[u8]>,
) -> Result<CommandOutput> {
    super::exec::run(
        ssh,
        command,
        input,
        Duration::from_secs(REQUEST_TIMEOUT_SECS),
    )
    .await?
    .ok_or_else(|| {
        ElevationError::CommandsRefused {
            host: host.to_owned(),
        }
        .into()
    })
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
    if !probe.sudo {
        return Err(ElevationError::NoSudo {
            host: host.to_owned(),
        }
        .into());
    }
    let server = probe.server.ok_or_else(|| ElevationError::NoSftpServer {
        host: host.to_owned(),
    })?;
    // Ask about this very program: a sudoers rule may let it run without a
    // password while everything else needs one, and then a password sent
    // on the SFTP channels would reach sftp-server instead of sudo.
    let answer = run(ssh, host, &password_probe(&server), None).await?;
    let stderr = String::from_utf8_lossy(&answer.stderr);
    let mode = if answer.status == Some(0) {
        SudoMode::NoPassword
    } else if needs_password(&stderr) {
        SudoMode::Password
    } else {
        // Refused outright, e.g. not in the sudoers file.
        let message = match sudo_reply(answer.status, &stderr) {
            SudoReply::Refused(message) => message,
            SudoReply::Accepted | SudoReply::WrongPassword => "it did not say why".to_owned(),
        };
        return Err(ElevationError::Refused {
            host: host.to_owned(),
            message,
        }
        .into());
    };
    tracing::debug!(?mode, "asked sudo whether it needs a password");
    if mode == SudoMode::NoPassword {
        return Ok(ElevatedCommand {
            command: format!("{SUDO} -n -- {server}"),
            password: None,
        });
    }
    let mut retry = false;
    for _ in 0..PASSWORD_ATTEMPTS {
        let password = ask(retry).await.ok_or(ElevationError::Cancelled)?;
        if password.expose_secret().contains(['\n', '\r', '\0']) {
            // sudo reads one line; the rest would go to sftp-server.
            return Err(ElevationError::Refused {
                host: host.to_owned(),
                message: "a password with a line break cannot be passed to sudo".into(),
            }
            .into());
        }
        // The program that will run, with no input: it ends at once, and
        // sudo's answer is the one the SFTP channels will get.
        let check = format!("{SUDO} -S -k -p \"\" -- {server}");
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
    Err(ElevationError::Refused {
        host: host.to_owned(),
        message: "the password was wrong three times".into(),
    }
    .into())
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
            "server /usr/lib/openssh/sftp-server\nserver /usr/lib/ssh/sftp-server\nsudo present\n",
        );
        assert_eq!(
            probe,
            Probe {
                server: Some("/usr/lib/openssh/sftp-server".into()),
                sudo: true,
            }
        );
        let probe = parse_probe("sudo present\n");
        assert_eq!(probe.server, None);
        assert!(probe.sudo);
        assert!(!parse_probe("sudo missing\n").sudo);
    }

    #[test]
    fn probe_skips_paths_that_would_need_quoting() {
        let probe = parse_probe(
            "server /opt/my server/sftp-server\nserver /bin/x;rm\nserver /bin/sh\n\
             server /usr/lib/ssh/sftp-server\n",
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
                       ferge-nosudo is not in the sudoers file.\n\
                       This incident has been reported to the administrator.\n";
        assert_eq!(
            sudo_reply(Some(1), lecture),
            SudoReply::Refused("\u{201c}ferge-nosudo is not in the sudoers file\u{201d}".into())
        );
        assert_eq!(
            sudo_reply(Some(1), "sudo: sorry, you must have a tty to run sudo\n"),
            SudoReply::Refused("\u{201c}sorry, you must have a tty to run sudo\u{201d}".into())
        );
    }

    #[test]
    fn server_text_is_quoted_short_and_plain() {
        let long = format!("evil\u{202e}txt\x1b[31m{}", "x".repeat(500));
        let SudoReply::Refused(message) = sudo_reply(Some(1), &long) else {
            panic!("refused expected");
        };
        assert!(!message.contains('\u{202e}') && !message.contains('\x1b'));
        assert_eq!(message.chars().count(), MESSAGE_LIMIT + 3);
        assert!(message.starts_with('\u{201c}') && message.ends_with("…\u{201d}"));
    }

    #[test]
    fn asks_sudo_about_the_program_itself() {
        assert_eq!(
            password_probe("/usr/lib/ssh/sftp-server"),
            "env LC_ALL=C sudo -n -k -- /usr/lib/ssh/sftp-server </dev/null"
        );
        assert!(needs_password("sudo: a password is required\n"));
        assert!(needs_password(
            "sudo: interactive authentication is required\n"
        ));
        assert!(!needs_password("sudo: user is not in the sudoers file\n"));
    }
}
