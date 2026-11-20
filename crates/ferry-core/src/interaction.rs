//! Questions the core has to ask the user while connecting.
//!
//! The core never talks to a UI directly. Instead, a connection receives an
//! implementation of [`Interaction`] and awaits its answers. A GUI forwards the
//! questions to dialogs; tests and benchmarks answer them programmatically.

use std::fmt;
use std::path::Path;

use async_trait::async_trait;
use secrecy::SecretString;

/// An unknown host key that needs the user's decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKeyPrompt {
    /// Host name as matched against known_hosts, e.g. `example.org` or `[example.org]:2222`.
    pub host: String,
    /// Key algorithm, e.g. `ssh-ed25519`.
    pub algorithm: String,
    /// SHA-256 fingerprint in OpenSSH notation (`SHA256:…`).
    pub fingerprint: String,
    /// Keys of other types are already recorded for this host. The new type is
    /// not necessarily suspicious, but deserves a closer look.
    pub other_types_known: bool,
}

/// The user's answer to a [`HostKeyPrompt`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostKeyDecision {
    /// Abort the connection.
    Reject,
    /// Trust the key for this connection only.
    AcceptOnce,
    /// Trust the key and record it in the user's known_hosts file.
    AcceptAndRemember,
}

/// One prompt of a keyboard-interactive authentication round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyboardPrompt {
    pub text: String,
    /// Whether the answer may be shown while typing (false for passwords and OTPs).
    pub echo: bool,
}

/// Answers questions that come up while connecting.
///
/// Returning `None` for a secret means "not available": the authentication
/// method is skipped and the next one is tried.
#[async_trait]
pub trait Interaction: Send + Sync + fmt::Debug {
    /// The server presented a host key that is not recorded anywhere.
    async fn confirm_host_key(&self, prompt: HostKeyPrompt) -> HostKeyDecision;

    /// A private key file is encrypted.
    async fn key_passphrase(&self, key_file: &Path, retry: bool) -> Option<SecretString>;

    /// The server accepts password authentication.
    async fn password(&self, user: &str, host: &str, retry: bool) -> Option<SecretString>;

    /// The server sent a keyboard-interactive challenge. Must return exactly one
    /// answer per prompt.
    async fn keyboard_interactive(
        &self,
        name: &str,
        instructions: &str,
        prompts: &[KeyboardPrompt],
    ) -> Option<Vec<SecretString>>;
}

/// An [`Interaction`] that never asks anyone: unknown host keys are rejected and
/// no secrets are provided. Suitable for non-interactive use.
#[derive(Clone, Copy, Debug, Default)]
pub struct NonInteractive;

#[async_trait]
impl Interaction for NonInteractive {
    async fn confirm_host_key(&self, _prompt: HostKeyPrompt) -> HostKeyDecision {
        HostKeyDecision::Reject
    }

    async fn key_passphrase(&self, _key_file: &Path, _retry: bool) -> Option<SecretString> {
        None
    }

    async fn password(&self, _user: &str, _host: &str, _retry: bool) -> Option<SecretString> {
        None
    }

    async fn keyboard_interactive(
        &self,
        _name: &str,
        _instructions: &str,
        _prompts: &[KeyboardPrompt],
    ) -> Option<Vec<SecretString>> {
        None
    }
}
