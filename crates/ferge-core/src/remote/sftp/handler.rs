//! The russh client handler: host key verification during the handshake.

use std::sync::{Arc, Mutex, PoisonError};

use russh::client;
use russh::keys::{PublicKey, PublicKeyOrCertificate};

use super::config::StrictHostKeys;
use super::known_hosts::{KnownHosts, Verdict, fingerprint};
use crate::error::HostKeyError;
use crate::interaction::{HostKeyDecision, HostKeyPrompt, Interaction};

/// Slot through which the handler reports why it rejected a host key; russh
/// itself only surfaces a generic "unknown key" error.
pub(crate) type HostKeyFailure = Arc<Mutex<Option<HostKeyError>>>;

pub(crate) struct ClientHandler {
    /// Name under which the host is recorded, e.g. `[host]:2222`.
    host_key_name: String,
    known_hosts: KnownHosts,
    strict: StrictHostKeys,
    interaction: Arc<dyn Interaction>,
    failure: HostKeyFailure,
}

impl ClientHandler {
    pub(crate) fn new(
        host_key_name: String,
        known_hosts: KnownHosts,
        strict: StrictHostKeys,
        interaction: Arc<dyn Interaction>,
        failure: HostKeyFailure,
    ) -> Self {
        Self {
            host_key_name,
            known_hosts,
            strict,
            interaction,
            failure,
        }
    }

    async fn verify(&self, key: &PublicKey) -> Result<(), HostKeyError> {
        let host = self.host_key_name.clone();
        let fingerprint = fingerprint(key);
        match self.known_hosts.check(&host, key).await? {
            Verdict::Trusted => Ok(()),
            Verdict::Revoked => Err(HostKeyError::Revoked { host, fingerprint }),
            Verdict::Changed { file, line } => Err(HostKeyError::Changed {
                host,
                fingerprint,
                file,
                line,
            }),
            Verdict::Unknown { .. } if self.strict == StrictHostKeys::Refuse => {
                Err(HostKeyError::Rejected { host, fingerprint })
            }
            Verdict::Unknown { .. } if self.strict == StrictHostKeys::AcceptNew => {
                tracing::info!(%host, %fingerprint, "recording a new host key (StrictHostKeyChecking accept-new)");
                self.known_hosts.remember(&host, key).await
            }
            Verdict::Unknown { other_types_known } => {
                let prompt = HostKeyPrompt {
                    host: host.clone(),
                    algorithm: key.algorithm().to_string(),
                    fingerprint: fingerprint.clone(),
                    other_types_known,
                };
                match self.interaction.confirm_host_key(prompt).await {
                    HostKeyDecision::Reject => Err(HostKeyError::Rejected { host, fingerprint }),
                    HostKeyDecision::AcceptOnce => Ok(()),
                    HostKeyDecision::AcceptAndRemember => {
                        self.known_hosts.remember(&host, key).await
                    }
                }
            }
        }
    }

    fn fail(&self, error: HostKeyError) {
        tracing::warn!(%error, "host key verification failed");
        *self.failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(error);
    }
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = server_public_key else {
            self.fail(HostKeyError::CertificateUnsupported {
                host: self.host_key_name.clone(),
            });
            return Ok(false);
        };
        // russh asks only during the first key exchange; later ones run
        // inside the session already encrypted to the key accepted here.
        match self.verify(key).await {
            Ok(()) => Ok(true),
            Err(error) => {
                self.fail(error);
                Ok(false)
            }
        }
    }
}
