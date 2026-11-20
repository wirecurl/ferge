//! Bridges the core's [`Interaction`] questions to dialogs on the UI thread.
//!
//! The core awaits answers on tokio; the UI receives [`PromptRequest`]s over a
//! runtime-agnostic channel and replies through a oneshot sender. A dropped
//! reply (dialog closed, window gone) counts as "rejected" or "not available".

use std::path::Path;

use async_trait::async_trait;
use ferry_core::interaction::{HostKeyDecision, HostKeyPrompt, Interaction, KeyboardPrompt};
use futures::channel::{mpsc, oneshot};
use secrecy::SecretString;

/// A question for the user.
#[derive(Debug)]
pub(crate) enum PromptRequest {
    HostKey {
        prompt: HostKeyPrompt,
        reply: oneshot::Sender<HostKeyDecision>,
    },
    Secret(SecretRequest),
}

/// A request for a password, passphrase or one-time code.
#[derive(Debug)]
pub(crate) struct SecretRequest {
    pub title: String,
    pub message: String,
    /// Whether the answer may be displayed while typing.
    pub echo: bool,
    pub reply: oneshot::Sender<Option<SecretString>>,
}

/// [`Interaction`] implementation that forwards every question to the UI.
#[derive(Debug, Clone)]
pub(crate) struct UiInteraction {
    sender: mpsc::UnboundedSender<PromptRequest>,
}

/// Creates the interaction and the receiving end for the UI.
pub(crate) fn channel() -> (UiInteraction, mpsc::UnboundedReceiver<PromptRequest>) {
    let (sender, receiver) = mpsc::unbounded();
    (UiInteraction { sender }, receiver)
}

impl UiInteraction {
    async fn secret(&self, title: String, message: String, echo: bool) -> Option<SecretString> {
        let (reply, answer) = oneshot::channel();
        let request = PromptRequest::Secret(SecretRequest {
            title,
            message,
            echo,
            reply,
        });
        self.sender.unbounded_send(request).ok()?;
        answer.await.ok().flatten()
    }
}

#[async_trait]
impl Interaction for UiInteraction {
    async fn confirm_host_key(&self, prompt: HostKeyPrompt) -> HostKeyDecision {
        let (reply, answer) = oneshot::channel();
        if self
            .sender
            .unbounded_send(PromptRequest::HostKey { prompt, reply })
            .is_err()
        {
            return HostKeyDecision::Reject;
        }
        answer.await.unwrap_or(HostKeyDecision::Reject)
    }

    async fn key_passphrase(&self, key_file: &Path, retry: bool) -> Option<SecretString> {
        let message = if retry {
            format!("Wrong passphrase. Try again for {}", key_file.display())
        } else {
            format!("Enter the passphrase for {}", key_file.display())
        };
        self.secret("Key passphrase".into(), message, false).await
    }

    async fn password(&self, user: &str, host: &str, retry: bool) -> Option<SecretString> {
        let message = if retry {
            format!("Wrong password. Try again for {user}@{host}")
        } else {
            format!("Password for {user}@{host}")
        };
        self.secret("Password".into(), message, false).await
    }

    async fn keyboard_interactive(
        &self,
        name: &str,
        instructions: &str,
        prompts: &[KeyboardPrompt],
    ) -> Option<Vec<SecretString>> {
        let title = if name.is_empty() {
            "Authentication".to_owned()
        } else {
            name.to_owned()
        };
        let mut answers = Vec::with_capacity(prompts.len());
        for prompt in prompts {
            let message = if instructions.is_empty() {
                prompt.text.clone()
            } else {
                format!("{instructions}\n{}", prompt.text)
            };
            answers.push(self.secret(title.clone(), message, prompt.echo).await?);
        }
        Some(answers)
    }
}
