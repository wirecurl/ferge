//! User authentication: SSH agent, key files, keyboard-interactive and password.
//!
//! The order follows OpenSSH: public keys first (agent keys, then key files),
//! then keyboard-interactive, then password. The server's list of acceptable
//! methods is honored after every attempt, which also handles multi-factor
//! setups that answer with `partial_success`.
//!
//! When [`Interaction`] returns `None` for a secret, the method is skipped; if
//! nothing else is left, authentication fails with
//! [`Error::AuthenticationFailed`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::MethodKind;
use russh::client::{self, AuthResult, KeyboardInteractiveAuthResponse};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg, PublicKey};
use secrecy::{ExposeSecret as _, SecretString};

use super::config::ResolvedTarget;
use super::handler::ClientHandler;
use crate::error::{Error, Result};
use crate::interaction::{Interaction, KeyboardPrompt};

/// How often a secret may be re-entered before giving up on a method.
const MAX_SECRET_ATTEMPTS: usize = 3;
/// Rounds of keyboard-interactive questions taken at most; a server that
/// asks again and again (or asks nothing, over and over) is not followed
/// forever.
const MAX_KEYBOARD_ROUNDS: usize = 10;

/// Servers disconnect after `MaxAuthTries` (default 6) failures. Stop offering
/// agent keys before that so password authentication still gets its turn.
const MAX_AGENT_KEYS: usize = 4;

type Handle = client::Handle<ClientHandler>;

/// Outcome of one method.
enum Step {
    Success,
    /// The method failed; the server lists what it accepts next.
    Continue(Vec<MethodKind>),
    /// Nothing was attempted (no keys, no password); the server's list is unchanged.
    Skipped,
}

impl From<AuthResult> for Step {
    fn from(result: AuthResult) -> Self {
        match result {
            AuthResult::Success => Self::Success,
            AuthResult::Failure {
                remaining_methods, ..
            } => Self::Continue(remaining_methods.to_vec()),
        }
    }
}

pub(crate) struct Authenticator<'a> {
    handle: &'a mut Handle,
    target: &'a ResolvedTarget,
    password: Option<&'a SecretString>,
    use_agent: bool,
    interaction: &'a Arc<dyn Interaction>,
}

impl<'a> Authenticator<'a> {
    pub(crate) fn new(
        handle: &'a mut Handle,
        target: &'a ResolvedTarget,
        password: Option<&'a SecretString>,
        use_agent: bool,
        interaction: &'a Arc<dyn Interaction>,
    ) -> Self {
        Self {
            handle,
            target,
            password,
            use_agent,
            interaction,
        }
    }

    pub(crate) async fn run(mut self) -> Result<()> {
        let mut allowed = match self
            .handle
            .authenticate_none(self.user())
            .await
            .map_err(Self::protocol)?
        {
            AuthResult::Success => return Ok(()),
            AuthResult::Failure {
                remaining_methods, ..
            } => remaining_methods.to_vec(),
        };
        let mut tried: Vec<MethodKind> = Vec::new();

        loop {
            let next = [
                MethodKind::PublicKey,
                MethodKind::KeyboardInteractive,
                MethodKind::Password,
            ]
            .into_iter()
            .find(|method| allowed.contains(method) && !tried.contains(method));
            let Some(method) = next else {
                return Err(Error::AuthenticationFailed {
                    user: self.target.user.clone(),
                    host: self.target.alias.clone(),
                });
            };
            tried.push(method);
            let step = match method {
                MethodKind::PublicKey => self.public_key().await?,
                MethodKind::KeyboardInteractive => self.keyboard_interactive().await?,
                MethodKind::Password => self.password().await?,
                _ => unreachable!("only the methods listed above are selected"),
            };
            match step {
                Step::Success => return Ok(()),
                Step::Continue(methods) if !methods.is_empty() => allowed = methods,
                // An empty list after a failure means the server gave up on us.
                Step::Continue(_) | Step::Skipped => {
                    if self.handle.is_closed() {
                        return Err(Error::Disconnected);
                    }
                }
            }
        }
    }

    fn user(&self) -> String {
        self.target.user.clone()
    }

    fn protocol(error: russh::Error) -> Error {
        Error::protocol("SSH authentication failed", error)
    }

    async fn public_key(&mut self) -> Result<Step> {
        let mut last = Step::Skipped;
        let mut offered: Vec<PublicKey> = Vec::new();

        if self.use_agent
            && let Some(step) = self.agent_keys(&mut offered).await?
        {
            match step {
                Step::Success => return Ok(Step::Success),
                Step::Continue(methods) if !methods.contains(&MethodKind::PublicKey) => {
                    return Ok(Step::Continue(methods));
                }
                other => last = other,
            }
        }

        for path in self.target.identity_files.clone() {
            let Some(key) = self.load_key(&path).await? else {
                continue;
            };
            if offered
                .iter()
                .any(|known| known.key_data() == key.public_key().key_data())
            {
                continue;
            }
            offered.push(key.public_key().clone());
            let hash = self.rsa_hash(&key).await?;
            let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
            let result = self
                .handle
                .authenticate_publickey(self.user(), key)
                .await
                .map_err(Self::protocol)?;
            match Step::from(result) {
                Step::Success => return Ok(Step::Success),
                Step::Continue(methods) if !methods.contains(&MethodKind::PublicKey) => {
                    return Ok(Step::Continue(methods));
                }
                other => last = other,
            }
        }
        Ok(last)
    }

    /// Offers the agent's keys. Keys matching configured identity files go first;
    /// with configured identities, other agent keys are not offered at all, which
    /// mirrors what users expect from a per-host `IdentityFile`. With
    /// `IdentitiesOnly yes`, only keys of the configured files are offered.
    async fn agent_keys(&mut self, offered: &mut Vec<PublicKey>) -> Result<Option<Step>> {
        let mut agent = match AgentClient::connect_env().await {
            Ok(agent) => agent,
            Err(error) => {
                tracing::debug!(%error, "no SSH agent available");
                return Ok(None);
            }
        };
        let identities = match agent.request_identities().await {
            Ok(identities) => identities,
            Err(error) => {
                tracing::warn!(%error, "could not list SSH agent keys");
                return Ok(None);
            }
        };
        let preferred = self.configured_public_keys().await;
        let mut candidates: Vec<PublicKey> = identities
            .iter()
            .filter_map(|identity| match identity {
                AgentIdentity::PublicKey { key, .. } => Some(key.clone()),
                // Certificates need `authenticate_certificate_with`; not supported yet.
                AgentIdentity::Certificate { .. } => None,
            })
            .collect();
        candidates.sort_by_key(|key| {
            !preferred
                .iter()
                .any(|wanted| wanted.key_data() == key.key_data())
        });
        // `IdentitiesOnly yes` allows only the configured keys, even if none of
        // them is in the agent; otherwise configured keys narrow the choice
        // when the agent holds any of them.
        if self.target.identities_only
            || (self.target.identities_configured && !preferred.is_empty())
        {
            candidates.retain(|key| {
                preferred
                    .iter()
                    .any(|wanted| wanted.key_data() == key.key_data())
            });
        }

        let mut last = None;
        for key in candidates.into_iter().take(MAX_AGENT_KEYS) {
            offered.push(key.clone());
            let hash = if key.algorithm().is_rsa() {
                self.handle
                    .best_supported_rsa_hash()
                    .await
                    .map_err(Self::protocol)?
                    .flatten()
            } else {
                None
            };
            let result = match self
                .handle
                .authenticate_publickey_with(self.user(), key, hash, &mut agent)
                .await
            {
                Ok(result) => result,
                Err(error) => {
                    tracing::warn!(%error, "SSH agent could not sign");
                    continue;
                }
            };
            match Step::from(result) {
                Step::Success => return Ok(Some(Step::Success)),
                Step::Continue(methods) if !methods.contains(&MethodKind::PublicKey) => {
                    return Ok(Some(Step::Continue(methods)));
                }
                other => last = Some(other),
            }
        }
        Ok(last)
    }

    /// Public halves (`.pub` files) of the configured identity files.
    async fn configured_public_keys(&self) -> Vec<PublicKey> {
        let mut keys = Vec::new();
        for path in &self.target.identity_files {
            let mut pub_path = path.clone().into_os_string();
            pub_path.push(".pub");
            if let Ok(text) = tokio::fs::read_to_string(PathBuf::from(pub_path)).await
                && let Ok(key) = PublicKey::from_openssh(text.trim())
            {
                keys.push(key);
            }
        }
        keys
    }

    async fn rsa_hash(&self, key: &PrivateKey) -> Result<Option<russh::keys::HashAlg>> {
        if key.algorithm().is_rsa() {
            Ok(self
                .handle
                .best_supported_rsa_hash()
                .await
                .map_err(Self::protocol)?
                .flatten())
        } else {
            Ok(None)
        }
    }

    /// Loads a private key, asking for its passphrase if needed. Returns `None`
    /// for missing or unreadable files and when the user declines.
    async fn load_key(&self, path: &Path) -> Result<Option<PrivateKey>> {
        let text = match tokio::fs::read_to_string(path).await {
            Ok(text) => SecretString::from(text),
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(key = %path.display(), %error, "cannot read key file");
                }
                return Ok(None);
            }
        };
        match russh::keys::decode_secret_key(text.expose_secret(), None) {
            Ok(key) => return Ok(Some(key)),
            Err(russh::keys::Error::KeyIsEncrypted) => {}
            Err(error) => {
                tracing::warn!(key = %path.display(), %error, "cannot parse key file");
                return Ok(None);
            }
        }
        for attempt in 0..MAX_SECRET_ATTEMPTS {
            let Some(passphrase) = self.interaction.key_passphrase(path, attempt > 0).await else {
                return Ok(None);
            };
            if let Ok(key) = russh::keys::decode_secret_key(
                text.expose_secret(),
                Some(passphrase.expose_secret()),
            ) {
                return Ok(Some(key));
            }
        }
        Ok(None)
    }

    async fn keyboard_interactive(&mut self) -> Result<Step> {
        let mut response = self
            .handle
            .authenticate_keyboard_interactive_start(self.user(), None)
            .await
            .map_err(Self::protocol)?;
        let mut failed_rounds: usize = 0;
        let mut rounds: usize = 0;
        loop {
            match response {
                KeyboardInteractiveAuthResponse::Success => return Ok(Step::Success),
                KeyboardInteractiveAuthResponse::Failure {
                    remaining_methods, ..
                } => {
                    failed_rounds = failed_rounds.saturating_add(1);
                    let methods = remaining_methods.to_vec();
                    if failed_rounds >= MAX_SECRET_ATTEMPTS
                        || !methods.contains(&MethodKind::KeyboardInteractive)
                    {
                        return Ok(Step::Continue(methods));
                    }
                    response = self
                        .handle
                        .authenticate_keyboard_interactive_start(self.user(), None)
                        .await
                        .map_err(Self::protocol)?;
                }
                KeyboardInteractiveAuthResponse::InfoRequest {
                    name,
                    instructions,
                    prompts,
                } => {
                    rounds += 1;
                    if rounds > MAX_KEYBOARD_ROUNDS {
                        tracing::debug!("the server kept asking; keyboard-interactive given up");
                        return Ok(Step::Skipped);
                    }
                    let answers = if prompts.is_empty() {
                        Vec::new()
                    } else if let Some(answer) =
                        self.single_password_answer(&prompts, failed_rounds)
                    {
                        vec![answer]
                    } else {
                        let prompts: Vec<KeyboardPrompt> = prompts
                            .into_iter()
                            .map(|prompt| KeyboardPrompt {
                                text: prompt.prompt,
                                echo: prompt.echo,
                            })
                            .collect();
                        match self
                            .interaction
                            .keyboard_interactive(
                                &self.target.account(),
                                &name,
                                &instructions,
                                &prompts,
                            )
                            .await
                        {
                            Some(answers) if answers.len() == prompts.len() => answers
                                .iter()
                                .map(|answer| answer.expose_secret().to_owned())
                                .collect(),
                            // Declined: answer with empty strings so the server
                            // fails this round and offers the next method.
                            _ => {
                                failed_rounds = MAX_SECRET_ATTEMPTS;
                                vec![String::new(); prompts.len()]
                            }
                        }
                    };
                    response = self
                        .handle
                        .authenticate_keyboard_interactive_respond(answers)
                        .await
                        .map_err(Self::protocol)?;
                }
            }
        }
    }

    /// Answers a lone hidden "Password:" prompt with the supplied password, so a
    /// pre-configured password also works on servers that only offer
    /// keyboard-interactive. Used only on the first round.
    fn single_password_answer(
        &self,
        prompts: &[russh::client::Prompt],
        failed_rounds: usize,
    ) -> Option<String> {
        let password = self.password?;
        let [prompt] = prompts else {
            return None;
        };
        (failed_rounds == 0
            && !prompt.echo
            && prompt.prompt.to_ascii_lowercase().contains("password"))
        .then(|| password.expose_secret().to_owned())
    }

    async fn password(&mut self) -> Result<Step> {
        let mut last = Step::Skipped;
        for attempt in 0..MAX_SECRET_ATTEMPTS {
            let password = match (attempt, self.password) {
                (0, Some(password)) => password.clone(),
                _ => match self
                    .interaction
                    .password(&self.target.account(), attempt > 0)
                    .await
                {
                    Some(password) => password,
                    None => return Ok(last),
                },
            };
            let result = self
                .handle
                .authenticate_password(self.user(), password.expose_secret())
                .await
                .map_err(Self::protocol)?;
            match Step::from(result) {
                Step::Success => return Ok(Step::Success),
                Step::Continue(methods) if !methods.contains(&MethodKind::Password) => {
                    return Ok(Step::Continue(methods));
                }
                other => last = other,
            }
            if self.handle.is_closed() {
                return Err(Error::Disconnected);
            }
        }
        Ok(last)
    }
}
