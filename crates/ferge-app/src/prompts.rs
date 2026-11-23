//! Questions from background work to the user.
//!
//! Connections run on tokio and ask through a [`Prompter`]: each question is
//! sent to the window together with the tab it belongs to, and answered
//! through a oneshot channel. A dropped reply (dialog dismissed, tab closed,
//! window gone) counts as "declined".

use ferge_core::interaction::{CertificatePrompt, HostKeyDecision, HostKeyPrompt, PlaintextPrompt};
use ferge_core::transfer::{Conflict, ConflictDecision, DirectRequest};
use futures::channel::{mpsc, oneshot};
use secrecy::SecretString;

/// Identifies the tab a question belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SessionId(pub(crate) u64);

/// A question for the user, or a message to show.
#[derive(Debug)]
pub(crate) enum PromptRequest {
    /// Whether to trust a server. Dropping the reply declines.
    Trust {
        question: TrustQuestion,
        reply: oneshot::Sender<HostKeyDecision>,
    },
    Secret(SecretRequest),
    MasterPassword(MasterPasswordRequest),
    /// A transfer would replace an existing file. Dropping the reply cancels
    /// the transfer.
    Conflict {
        conflict: Conflict,
        reply: oneshot::Sender<ConflictDecision>,
    },
    /// Two servers could copy directly; may ferge set that up? Dropping the
    /// reply copies through this computer.
    Direct {
        request: DirectRequest,
        reply: oneshot::Sender<DirectAnswer>,
    },
    /// A problem that does not stop the current work, shown as a notification.
    Notice(String),
}

/// What a server presented that ferge cannot vouch for.
#[derive(Debug)]
pub(crate) enum TrustQuestion {
    /// An SSH host key not in known_hosts.
    HostKey(HostKeyPrompt),
    /// A TLS certificate the system does not trust.
    Certificate(CertificatePrompt),
    /// A server without encryption.
    Plaintext(PlaintextPrompt),
}

/// The answer to [`PromptRequest::Direct`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DirectAnswer {
    pub allow: bool,
    /// Give the same answer for these two servers until ferge quits.
    pub remember: bool,
}

/// A request for a password, passphrase or one-time code.
#[derive(Debug)]
pub(crate) struct SecretRequest {
    pub title: String,
    pub message: String,
    /// Whether the answer may be displayed while typing.
    pub echo: bool,
    /// Offer "Remember" with this initial state; `None` hides the option.
    pub remember: Option<bool>,
    /// Label of the button that submits the answer.
    pub confirm: &'static str,
    pub reply: oneshot::Sender<Option<SecretAnswer>>,
}

/// What the user entered into a [`SecretRequest`].
#[derive(Debug)]
pub(crate) struct SecretAnswer {
    pub secret: SecretString,
    pub remember: bool,
}

/// A request for the master password of the encrypted password file.
#[derive(Debug)]
pub(crate) struct MasterPasswordRequest {
    /// No file exists yet: the password is new and must be entered twice.
    pub create: bool,
    /// The previous attempt was wrong.
    pub retry: bool,
    pub reply: oneshot::Sender<Option<SecretString>>,
}

/// A [`PromptRequest`] and the tab it came from.
#[derive(Debug)]
pub(crate) struct TaggedPrompt {
    pub session: Option<SessionId>,
    pub request: PromptRequest,
}

/// Sends questions to the window on behalf of one tab (or none).
#[derive(Clone, Debug)]
pub(crate) struct Prompter {
    sender: mpsc::UnboundedSender<TaggedPrompt>,
    session: Option<SessionId>,
}

/// Creates a channel for prompts; the window owns the receiver.
pub(crate) fn channel() -> (
    mpsc::UnboundedSender<TaggedPrompt>,
    mpsc::UnboundedReceiver<TaggedPrompt>,
) {
    mpsc::unbounded()
}

impl Prompter {
    pub(crate) fn new(
        sender: mpsc::UnboundedSender<TaggedPrompt>,
        session: Option<SessionId>,
    ) -> Self {
        Self { sender, session }
    }

    fn send(&self, request: PromptRequest) -> bool {
        self.sender
            .unbounded_send(TaggedPrompt {
                session: self.session,
                request,
            })
            .is_ok()
    }

    pub(crate) async fn trust(&self, question: TrustQuestion) -> HostKeyDecision {
        let (reply, answer) = oneshot::channel();
        if !self.send(PromptRequest::Trust { question, reply }) {
            return HostKeyDecision::Reject;
        }
        answer.await.unwrap_or(HostKeyDecision::Reject)
    }

    pub(crate) async fn secret(
        &self,
        title: String,
        message: String,
        echo: bool,
        remember: Option<bool>,
    ) -> Option<SecretAnswer> {
        self.secret_for(title, message, echo, remember, "Log in")
            .await
    }

    /// Like [`Prompter::secret`], for something other than logging in.
    pub(crate) async fn secret_for(
        &self,
        title: String,
        message: String,
        echo: bool,
        remember: Option<bool>,
        confirm: &'static str,
    ) -> Option<SecretAnswer> {
        let (reply, answer) = oneshot::channel();
        let request = PromptRequest::Secret(SecretRequest {
            title,
            message,
            echo,
            remember,
            confirm,
            reply,
        });
        if !self.send(request) {
            return None;
        }
        answer.await.ok().flatten()
    }

    pub(crate) async fn master_password(&self, create: bool, retry: bool) -> Option<SecretString> {
        let (reply, answer) = oneshot::channel();
        let request = PromptRequest::MasterPassword(MasterPasswordRequest {
            create,
            retry,
            reply,
        });
        if !self.send(request) {
            return None;
        }
        answer.await.ok().flatten()
    }

    pub(crate) async fn conflict(&self, conflict: Conflict) -> Option<ConflictDecision> {
        let (reply, answer) = oneshot::channel();
        if !self.send(PromptRequest::Conflict { conflict, reply }) {
            return None;
        }
        answer.await.ok()
    }

    /// `None` if the question could not be asked or was dismissed.
    pub(crate) async fn direct(&self, request: DirectRequest) -> Option<DirectAnswer> {
        let (reply, answer) = oneshot::channel();
        if !self.send(PromptRequest::Direct { request, reply }) {
            return None;
        }
        answer.await.ok()
    }

    pub(crate) fn notice(&self, message: String) {
        self.send(PromptRequest::Notice(message));
    }
}
