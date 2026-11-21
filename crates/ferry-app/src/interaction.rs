//! Answers the core's [`Interaction`] questions for one connection attempt:
//! saved secrets first, then dialogs.
//!
//! Secrets the user asks to remember are only saved once the connection has
//! succeeded ([`UiInteraction::commit`]), so a mistyped password never ends
//! up in the keyring. A saved password the server rejects is replaced by the
//! one entered instead, or forgotten if the user unticks "Remember".

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use ferry_core::interaction::{
    Account, HostKeyDecision, HostKeyPrompt, Interaction, KeyboardPrompt,
};
use ferry_core::secrets::SecretId;
use secrecy::SecretString;

use crate::prompts::Prompter;
use crate::vault::Vault;

/// A change to the saved secrets, applied after a successful login.
#[derive(Debug)]
enum Change {
    Save(SecretString),
    Forget,
}

#[derive(Debug, Default)]
struct State {
    /// Secrets handed out from the vault in this attempt.
    used_saved: Vec<SecretId>,
    pending: Vec<(SecretId, Change)>,
    /// Keyboard-interactive password rounds answered so far.
    keyboard_password_rounds: usize,
    /// Where the sudo password last handed out came from.
    sudo_last: Option<SudoOffer>,
    sudo_checked_saved: bool,
    sudo_checked_login: bool,
    /// The saved sudo password was wrong.
    sudo_saved_rejected: bool,
}

/// Where a sudo password came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SudoOffer {
    Saved,
    Login,
    Typed,
}

/// [`Interaction`] for one connection attempt.
#[derive(Debug)]
pub(crate) struct UiInteraction {
    prompter: Prompter,
    vault: Arc<Vault>,
    state: Mutex<State>,
}

impl UiInteraction {
    pub(crate) fn new(prompter: Prompter, vault: Arc<Vault>) -> Self {
        Self {
            prompter,
            vault,
            state: Mutex::default(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Applies the remembered and forgotten secrets. Call after the
    /// connection succeeded; must run on the I/O runtime.
    pub(crate) async fn commit(&self) {
        let pending = std::mem::take(&mut self.state().pending);
        for (id, change) in pending {
            match change {
                Change::Save(secret) => self.vault.save(id, secret, &self.prompter).await,
                Change::Forget => self.vault.forget(id, &self.prompter).await,
            }
        }
    }

    /// Asks for a password or passphrase identified by `id`, preferring a
    /// saved one on the first attempt.
    async fn saved_or_asked(
        &self,
        id: SecretId,
        retry: bool,
        title: &str,
        ask: &str,
    ) -> Option<SecretString> {
        if !retry && let Some(saved) = self.vault.lookup(&id, &self.prompter).await {
            self.state().used_saved.push(id);
            return Some(saved);
        }
        let saved_rejected = {
            let mut state = self.state();
            let position = state.used_saved.iter().position(|used| used == &id);
            position
                .map(|index| state.used_saved.remove(index))
                .is_some()
        };
        let message = if saved_rejected {
            format!("The saved {title} was not accepted. {ask}")
        } else if retry {
            format!("That did not work. {ask}")
        } else {
            ask.to_owned()
        };
        let answer = self
            .prompter
            .secret(capitalize(title), message, false, Some(saved_rejected))
            .await?;
        let mut state = self.state();
        state.pending.retain(|(pending, _)| pending != &id);
        if answer.remember {
            state
                .pending
                .push((id, Change::Save(answer.secret.clone())));
        } else if saved_rejected {
            state.pending.push((id, Change::Forget));
        }
        Some(answer.secret)
    }
}

#[async_trait]
impl Interaction for UiInteraction {
    async fn confirm_host_key(&self, prompt: HostKeyPrompt) -> HostKeyDecision {
        self.prompter.host_key(prompt).await
    }

    async fn key_passphrase(&self, key_file: &Path, retry: bool) -> Option<SecretString> {
        let ask = format!("Enter the passphrase of {}.", key_file.display());
        self.saved_or_asked(
            SecretId::KeyPassphrase(key_file.to_path_buf()),
            retry,
            "key passphrase",
            &ask,
        )
        .await
    }

    async fn password(&self, account: &Account, retry: bool) -> Option<SecretString> {
        let ask = format!("Enter the password for {account}.");
        self.saved_or_asked(account.password_id(), retry, "password", &ask)
            .await
    }

    async fn keyboard_interactive(
        &self,
        account: &Account,
        name: &str,
        instructions: &str,
        prompts: &[KeyboardPrompt],
    ) -> Option<Vec<SecretString>> {
        // A lone hidden "Password:" prompt is a password in disguise; treat it
        // like one so it can be saved and filled in.
        if let [prompt] = prompts
            && !prompt.echo
            && prompt.text.to_lowercase().contains("password")
        {
            let retry = {
                let mut state = self.state();
                state.keyboard_password_rounds += 1;
                state.keyboard_password_rounds > 1
            };
            return self
                .password(account, retry)
                .await
                .map(|secret| vec![secret]);
        }
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
            let answer = self
                .prompter
                .secret(title.clone(), message, prompt.echo, None)
                .await?;
            answers.push(answer.secret);
        }
        Some(answers)
    }

    async fn sudo_password(&self, account: &Account, retry: bool) -> Option<SecretString> {
        self.sudo(account, retry).await
    }
}

impl UiInteraction {
    /// The password sudo asks for: a saved sudo password, else the saved
    /// login password (sudo usually wants the account's own), else asked.
    /// `retry`: the previous answer was wrong.
    async fn sudo(&self, account: &Account, retry: bool) -> Option<SecretString> {
        let id = account.sudo_password_id();
        let last = self.state().sudo_last.take();
        if retry && last == Some(SudoOffer::Saved) {
            // Forgotten once the login works, unless a new one is remembered.
            let mut state = self.state();
            state.sudo_saved_rejected = true;
            state.pending.push((id.clone(), Change::Forget));
        }
        let check_saved = !std::mem::replace(&mut self.state().sudo_checked_saved, true);
        if check_saved && let Some(saved) = self.vault.lookup(&id, &self.prompter).await {
            self.state().sudo_last = Some(SudoOffer::Saved);
            return Some(saved);
        }
        let check_login = !std::mem::replace(&mut self.state().sudo_checked_login, true);
        if check_login
            && let Some(login) = self
                .vault
                .lookup(&account.password_id(), &self.prompter)
                .await
        {
            self.state().sudo_last = Some(SudoOffer::Login);
            return Some(login);
        }
        let saved_rejected = self.state().sudo_saved_rejected;
        let ask = format!(
            "Enter the password sudo asks for on {}, usually your own.",
            account.host
        );
        let message = match last {
            Some(SudoOffer::Typed) => format!("That did not work. {ask}"),
            _ if saved_rejected => format!("The saved sudo password was not accepted. {ask}"),
            _ => ask,
        };
        let answer = self
            .prompter
            .secret_for(
                "Work as root".to_owned(),
                message,
                false,
                Some(saved_rejected),
                "Continue",
            )
            .await?;
        let mut state = self.state();
        state.sudo_last = Some(SudoOffer::Typed);
        state.pending.retain(|(pending, _)| pending != &id);
        if answer.remember {
            state
                .pending
                .push((id, Change::Save(answer.secret.clone())));
        } else if saved_rejected {
            state.pending.push((id, Change::Forget));
        }
        Some(answer.secret)
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

#[cfg(test)]
mod tests {
    use futures::StreamExt as _;
    use secrecy::ExposeSecret as _;

    use super::*;
    use crate::prompts::{self, PromptRequest, SecretAnswer};

    fn account() -> Account {
        Account {
            user: "me".into(),
            host: "server".into(),
            port: 22,
        }
    }

    /// Answers every secret prompt with `answer`, remembering as given.
    fn answering(
        answer: &'static str,
        remember: bool,
    ) -> (Prompter, tokio::task::JoinHandle<Vec<bool>>) {
        let (sender, mut receiver) = prompts::channel();
        let answers = tokio::spawn(async move {
            let mut asked = Vec::new();
            while let Some(prompt) = receiver.next().await {
                if let PromptRequest::Secret(request) = prompt.request {
                    asked.push(request.remember.unwrap_or(false));
                    let _ = request.reply.send(Some(SecretAnswer {
                        secret: SecretString::from(answer),
                        remember,
                    }));
                }
            }
            asked
        });
        (Prompter::new(sender, None), answers)
    }

    #[tokio::test]
    async fn remembers_only_after_commit_and_prefers_saved_secrets() {
        let vault = Vault::ephemeral();
        let (prompter, prompts) = answering("typed", true);
        let first = UiInteraction::new(prompter.clone(), Arc::clone(&vault));
        let typed = first.password(&account(), false).await.unwrap();
        assert_eq!(typed.expose_secret(), "typed");
        assert!(
            vault
                .lookup(&account().password_id(), &prompter)
                .await
                .is_none()
        );
        first.commit().await;

        let second = UiInteraction::new(prompter.clone(), Arc::clone(&vault));
        let saved = second.password(&account(), false).await.unwrap();
        assert_eq!(saved.expose_secret(), "typed");
        drop((first, second, prompter));
        // Only the first attempt asked; "Remember" started unticked.
        assert_eq!(prompts.await.unwrap(), [false]);
    }

    #[tokio::test]
    async fn forgets_a_rejected_saved_password_unless_remembered() {
        let vault = Vault::ephemeral();
        let (prompter, prompts) = answering("new", false);
        vault
            .save(
                account().password_id(),
                SecretString::from("old"),
                &prompter,
            )
            .await;
        let interaction = UiInteraction::new(prompter.clone(), Arc::clone(&vault));
        let saved = interaction.password(&account(), false).await.unwrap();
        assert_eq!(saved.expose_secret(), "old");
        // The server rejected it; the retry asks with "Remember" ticked.
        let typed = interaction.password(&account(), true).await.unwrap();
        assert_eq!(typed.expose_secret(), "new");
        interaction.commit().await;
        assert!(
            vault
                .lookup(&account().password_id(), &prompter)
                .await
                .is_none()
        );
        drop((interaction, prompter));
        assert_eq!(prompts.await.unwrap(), [true]);
    }

    #[tokio::test]
    async fn sudo_tries_the_saved_login_password_before_asking() {
        let vault = Vault::ephemeral();
        let (prompter, prompts) = answering("typed", true);
        vault
            .save(
                account().password_id(),
                SecretString::from("login"),
                &prompter,
            )
            .await;
        let interaction = UiInteraction::new(prompter.clone(), Arc::clone(&vault));
        let first = interaction.sudo_password(&account(), false).await.unwrap();
        assert_eq!(first.expose_secret(), "login");
        // sudo wants another password: ask, and remember it once it worked.
        let typed = interaction.sudo_password(&account(), true).await.unwrap();
        assert_eq!(typed.expose_secret(), "typed");
        interaction.commit().await;
        let saved = vault.lookup(&account().sudo_password_id(), &prompter).await;
        assert_eq!(saved.unwrap().expose_secret(), "typed");
        drop((interaction, prompter));
        assert_eq!(prompts.await.unwrap(), [false]);
    }

    #[tokio::test]
    async fn sudo_forgets_a_rejected_saved_password_unless_remembered() {
        let vault = Vault::ephemeral();
        let (prompter, prompts) = answering("new", false);
        vault
            .save(
                account().sudo_password_id(),
                SecretString::from("old"),
                &prompter,
            )
            .await;
        let interaction = UiInteraction::new(prompter.clone(), Arc::clone(&vault));
        let saved = interaction.sudo_password(&account(), false).await.unwrap();
        assert_eq!(saved.expose_secret(), "old");
        let typed = interaction.sudo_password(&account(), true).await.unwrap();
        assert_eq!(typed.expose_secret(), "new");
        interaction.commit().await;
        assert!(
            vault
                .lookup(&account().sudo_password_id(), &prompter)
                .await
                .is_none()
        );
        drop((interaction, prompter));
        // Asked once, with "Remember" ticked because a saved one existed.
        assert_eq!(prompts.await.unwrap(), [true]);
    }
}
