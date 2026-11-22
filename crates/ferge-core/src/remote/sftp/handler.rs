//! The russh client handler: host key verification during the handshake.

use std::sync::{Arc, Mutex, PoisonError};

use russh::client;
use russh::keys::{PublicKey, PublicKeyOrCertificate};

use super::known_hosts::{KnownHosts, Verdict, fingerprint};
use crate::error::HostKeyError;
use crate::interaction::{HostKeyDecision, HostKeyPrompt, Interaction};

/// Slot through which the handler reports why it rejected a host key; russh
/// itself only surfaces a generic "unknown key" error.
pub(crate) type HostKeyFailure = Arc<Mutex<Option<HostKeyError>>>;

/// Slot that receives the host key once it was accepted, so the session can
/// hand it on (direct transfers tell the other server which key to expect).
pub(crate) type AcceptedHostKey = Arc<Mutex<Option<PublicKey>>>;

pub(crate) struct ClientHandler {
    /// Name under which the host is recorded, e.g. `[host]:2222`.
    host_key_name: String,
    known_hosts: KnownHosts,
    interaction: Arc<dyn Interaction>,
    failure: HostKeyFailure,
    /// Key accepted during the initial handshake; re-keying must present the same.
    accepted: Option<PublicKey>,
    reported: AcceptedHostKey,
}

impl ClientHandler {
    pub(crate) fn new(
        host_key_name: String,
        known_hosts: KnownHosts,
        interaction: Arc<dyn Interaction>,
        failure: HostKeyFailure,
        reported: AcceptedHostKey,
    ) -> Self {
        Self {
            host_key_name,
            known_hosts,
            interaction,
            failure,
            accepted: None,
            reported,
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
        if let Some(accepted) = &self.accepted {
            // Re-key: the server must keep its identity.
            let same = accepted.key_data() == key.key_data();
            if !same {
                self.fail(HostKeyError::Changed {
                    host: self.host_key_name.clone(),
                    fingerprint: fingerprint(key),
                    file: "(current session)".into(),
                    line: 0,
                });
            }
            return Ok(same);
        }
        match self.verify(key).await {
            Ok(()) => {
                self.accepted = Some(key.clone());
                *self.reported.lock().unwrap_or_else(PoisonError::into_inner) = Some(key.clone());
                Ok(true)
            }
            Err(error) => {
                self.fail(error);
                Ok(false)
            }
        }
    }
}
