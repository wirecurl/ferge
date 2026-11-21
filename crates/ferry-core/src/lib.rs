//! UI-independent core of Ferry.
//!
//! Everything that touches the network or the file system lives here and runs
//! on tokio. The crate knows nothing about windows or widgets: it exposes
//! asynchronous operations, streams of results and the [`Interaction`] trait for
//! questions that need a human.
//!
//! [`Interaction`]: interaction::Interaction

pub mod destination;
pub mod entry;
pub mod error;
mod glob;
pub mod interaction;
pub mod library;
pub mod listing;
pub mod path;
pub mod remote;

pub use destination::{Destination, SftpTarget};
pub use entry::{Entry, EntryKind, Metadata};
pub use error::{Error, HostKeyError, Result};
pub use path::{EntryName, RemotePath};
pub use remote::Remote;
