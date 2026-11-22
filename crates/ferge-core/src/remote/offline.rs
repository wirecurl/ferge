//! A side of a transfer that is not connected (yet): ferge restored the
//! transfer after a restart and connects only when it is resumed. Every
//! operation fails with [`Error::Disconnected`].

use std::time::SystemTime;

use async_trait::async_trait;
use futures::StreamExt as _;

use super::{ByteStream, FileWriter, ListStream, Remote};
use crate::entry::Metadata;
use crate::error::{Error, Result};
use crate::path::RemotePath;

/// A placeholder for a file system or server that is not connected.
#[derive(Debug)]
pub struct Offline {
    label: String,
    local: bool,
}

impl Offline {
    /// Stands in for `label`; `local` if it is this computer.
    pub fn new(label: impl Into<String>, local: bool) -> Self {
        Self {
            label: label.into(),
            local,
        }
    }
}

#[async_trait]
impl Remote for Offline {
    fn label(&self) -> &str {
        &self.label
    }

    fn is_local(&self) -> bool {
        self.local
    }

    async fn home_dir(&self) -> Result<RemotePath> {
        Err(Error::Disconnected)
    }

    async fn canonicalize(&self, _path: &RemotePath) -> Result<RemotePath> {
        Err(Error::Disconnected)
    }

    fn list(&self, _path: &RemotePath) -> ListStream {
        futures::stream::once(async { Err(Error::Disconnected) }).boxed()
    }

    async fn stat(&self, _path: &RemotePath) -> Result<Metadata> {
        Err(Error::Disconnected)
    }

    async fn read_from(&self, _path: &RemotePath, _offset: u64) -> Result<ByteStream> {
        Err(Error::Disconnected)
    }

    async fn open_writer(&self, _path: &RemotePath, _offset: u64) -> Result<Box<dyn FileWriter>> {
        Err(Error::Disconnected)
    }

    async fn rename(&self, _from: &RemotePath, _to: &RemotePath) -> Result<()> {
        Err(Error::Disconnected)
    }

    async fn rename_replace(&self, _from: &RemotePath, _to: &RemotePath) -> Result<()> {
        Err(Error::Disconnected)
    }

    async fn set_modified(&self, _path: &RemotePath, _time: SystemTime) -> Result<()> {
        Err(Error::Disconnected)
    }

    async fn delete_file(&self, _path: &RemotePath) -> Result<()> {
        Err(Error::Disconnected)
    }

    async fn delete_dir(&self, _path: &RemotePath) -> Result<()> {
        Err(Error::Disconnected)
    }

    async fn mkdir(&self, _path: &RemotePath) -> Result<()> {
        Err(Error::Disconnected)
    }

    async fn chmod(&self, _path: &RemotePath, _mode: u32) -> Result<()> {
        Err(Error::Disconnected)
    }
}
