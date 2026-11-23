//! Connections for transfers outside any tab: to follow a direct job again
//! after a restart, to resume a saved transfer, and to go on after a lost
//! connection.

use std::path::PathBuf;
use std::sync::Arc;

use ferge_core::connect::{self, ConnectOptions};
use ferge_core::interaction::Interaction;
use ferge_core::{Destination, Remote};
use futures::channel::mpsc;

use crate::interaction::UiInteraction;
use crate::prompts::{Prompter, TaggedPrompt};
use crate::vault::Vault;

/// Connects like a tab does: the SSH configuration ferge was started with,
/// saved passwords, and questions in the window's dialogs.
#[derive(Clone, Debug)]
pub(crate) struct Connector {
    prompts: mpsc::UnboundedSender<TaggedPrompt>,
    vault: Arc<Vault>,
    ssh_config: Option<PathBuf>,
}

impl Connector {
    pub(crate) fn new(
        prompts: mpsc::UnboundedSender<TaggedPrompt>,
        vault: Arc<Vault>,
        ssh_config: Option<PathBuf>,
    ) -> Self {
        Self {
            prompts,
            vault,
            ssh_config,
        }
    }

    /// Shows a notification in the window.
    pub(crate) fn notice(&self, message: String) {
        Prompter::new(self.prompts.clone(), None).notice(message);
    }

    /// Connects to `destination`. Must run on the I/O runtime.
    pub(crate) async fn connect(
        &self,
        destination: &Destination,
    ) -> ferge_core::Result<Arc<dyn Remote>> {
        let interaction = Arc::new(UiInteraction::new(
            Prompter::new(self.prompts.clone(), None),
            Arc::clone(&self.vault),
        ));
        let options = ConnectOptions {
            ssh_config: self.ssh_config.clone(),
            ..ConnectOptions::default()
        };
        let connection = connect::connect(
            destination,
            &options,
            Arc::clone(&interaction) as Arc<dyn Interaction>,
        )
        .await?;
        interaction.commit().await;
        Ok(connection.remote)
    }

    /// Connects to both sides of a transfer at once.
    pub(crate) async fn connect_both(
        &self,
        source: Option<&Destination>,
        target: Option<&Destination>,
    ) -> ferge_core::Result<(Arc<dyn Remote>, Arc<dyn Remote>)> {
        let (Some(source), Some(target)) = (source, target) else {
            return Err(ferge_core::Error::Unsupported(
                "connecting again to servers ferge did not record",
            ));
        };
        // Boxed: two connection attempts make a large future.
        let (source, target) = futures::join!(
            Box::pin(self.connect(source)),
            Box::pin(self.connect(target))
        );
        Ok((source?, target?))
    }
}

/// Whether trying again later may help: the network or the server was not
/// there, rather than a refused login.
pub(crate) fn worth_retrying(error: &ferge_core::Error) -> bool {
    error.is_connection_lost() || matches!(error, ferge_core::Error::Connect { .. })
}
