//! A time limit for connecting that leaves out the time spent on questions.
//!
//! A server (or a `ProxyCommand`) that accepts the connection and then goes
//! silent, or sends banner lines one by one, would otherwise keep
//! "Connecting…" up forever. The user may take as long as they like to
//! compare a fingerprint or type a password, so only the time ferge itself
//! waits counts against the limit (`SftpOptions::handshake_timeout`).

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

use async_trait::async_trait;
use secrecy::SecretString;

use crate::error::{Error, Result};
use crate::interaction::{Account, HostKeyDecision, HostKeyPrompt, Interaction, KeyboardPrompt};

/// How often the limit is looked at while a question is open.
const QUESTION_CHECK: Duration = Duration::from_secs(1);

/// Measures the time spent waiting for answers.
#[derive(Debug, Default)]
pub(super) struct Clock {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// Time spent on questions answered so far.
    asked: Duration,
    /// When the question being asked now was asked.
    asking_since: Option<Instant>,
}

impl Clock {
    /// Time spent on questions, the current one included, and whether one
    /// is open now.
    fn asked(&self) -> (Duration, bool) {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let open = state.asking_since.map(|since| since.elapsed());
        (state.asked + open.unwrap_or(Duration::ZERO), open.is_some())
    }

    async fn asking<T>(&self, question: impl Future<Output = T>) -> T {
        let started = Instant::now();
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .asking_since = Some(started);
        let answer = question.await;
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.asking_since = None;
        state.asked += started.elapsed();
        answer
    }

    /// Runs `work` until it is done or has spent `limit` outside of
    /// questions.
    pub(super) async fn limit_to<T>(
        &self,
        limit: Duration,
        work: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        let started = Instant::now();
        let mut work = std::pin::pin!(work);
        loop {
            let (asked, asking) = self.asked();
            let used = started.elapsed().saturating_sub(asked);
            let left = limit.saturating_sub(used);
            if left.is_zero() && !asking {
                return Err(Error::Timeout("connecting"));
            }
            // While a question is open the clock stands still: look again
            // in a while rather than at once.
            let wait = if asking {
                left.max(QUESTION_CHECK)
            } else {
                left
            };
            if let Ok(result) = tokio::time::timeout(wait, &mut work).await {
                return result;
            }
        }
    }
}

/// An [`Interaction`] whose questions are timed by a [`Clock`].
#[derive(Debug)]
pub(super) struct Timed {
    pub(super) inner: Arc<dyn Interaction>,
    pub(super) clock: Arc<Clock>,
}

#[async_trait]
impl Interaction for Timed {
    async fn confirm_host_key(&self, prompt: HostKeyPrompt) -> HostKeyDecision {
        self.clock.asking(self.inner.confirm_host_key(prompt)).await
    }

    async fn key_passphrase(&self, key_file: &Path, retry: bool) -> Option<SecretString> {
        self.clock
            .asking(self.inner.key_passphrase(key_file, retry))
            .await
    }

    async fn password(&self, account: &Account, retry: bool) -> Option<SecretString> {
        self.clock.asking(self.inner.password(account, retry)).await
    }

    async fn keyboard_interactive(
        &self,
        account: &Account,
        name: &str,
        instructions: &str,
        prompts: &[KeyboardPrompt],
    ) -> Option<Vec<SecretString>> {
        self.clock
            .asking(
                self.inner
                    .keyboard_interactive(account, name, instructions, prompts),
            )
            .await
    }

    async fn sudo_password(&self, account: &Account, retry: bool) -> Option<SecretString> {
        self.clock
            .asking(self.inner.sudo_password(account, retry))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn silence_times_out_but_questions_do_not_count() {
        let clock = Clock::default();
        let silent = clock
            .limit_to(Duration::from_secs(5), std::future::pending::<Result<()>>())
            .await;
        assert!(matches!(silent, Err(Error::Timeout(_))));

        // Four seconds of work, a minute on a question, four more seconds.
        let clock = Clock::default();
        let work = async {
            tokio::time::sleep(Duration::from_secs(4)).await;
            clock
                .asking(tokio::time::sleep(Duration::from_secs(60)))
                .await;
            tokio::time::sleep(Duration::from_secs(4)).await;
            Ok(7)
        };
        assert_eq!(
            clock.limit_to(Duration::from_secs(10), work).await.ok(),
            Some(7)
        );

        let clock = Clock::default();
        let work = async {
            clock
                .asking(tokio::time::sleep(Duration::from_secs(60)))
                .await;
            tokio::time::sleep(Duration::from_secs(20)).await;
            Ok(())
        };
        assert!(matches!(
            clock.limit_to(Duration::from_secs(10), work).await,
            Err(Error::Timeout(_))
        ));
    }
}
