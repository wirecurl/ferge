//! Connecting to a [`Destination`], whatever protocol it speaks: one place
//! for the tabs, for transfers that resume after a restart, and for
//! transfers that go on after a lost connection.

use std::path::PathBuf;
use std::sync::Arc;

use secrecy::SecretString;

use crate::destination::Destination;
use crate::error::Result;
use crate::interaction::Interaction;
use crate::remote::Remote;
use crate::remote::local::LocalRemote;
use crate::remote::sftp::{SftpRemote, SshConfigSource};

/// What a connection needs besides its destination.
#[derive(Clone, Debug, Default)]
pub struct ConnectOptions {
    /// The `ssh_config` to read instead of `~/.ssh/config`.
    pub ssh_config: Option<PathBuf>,
    /// A private key to try before the configured ones.
    pub identity_file: Option<PathBuf>,
    /// A password to use without asking (Quick Connect).
    pub password: Option<SecretString>,
}

/// An established connection.
#[derive(Clone, Debug)]
pub struct Connection {
    pub remote: Arc<dyn Remote>,
    /// The SSH session behind `remote`, which can open a second one as root.
    pub sftp: Option<SftpRemote>,
}

/// Connects to `destination`. Questions (host keys, passwords) go to
/// `interaction`.
pub async fn connect(
    destination: &Destination,
    options: &ConnectOptions,
    interaction: Arc<dyn Interaction>,
) -> Result<Connection> {
    match destination {
        Destination::Local { .. } => Ok(Connection {
            remote: Arc::new(LocalRemote::new()),
            sftp: None,
        }),
        Destination::Sftp(target) => {
            let mut sftp_options = target.options();
            if let Some(file) = &options.ssh_config {
                sftp_options.ssh_config = SshConfigSource::File(file.clone());
            }
            sftp_options
                .identity_files
                .extend(options.identity_file.clone());
            sftp_options.password.clone_from(&options.password);
            let sftp = SftpRemote::connect(sftp_options, interaction).await?;
            Ok(Connection {
                remote: Arc::new(sftp.clone()),
                sftp: Some(sftp),
            })
        }
    }
}
