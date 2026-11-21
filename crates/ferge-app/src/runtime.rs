//! The tokio runtime that runs all network and file system work.
//!
//! GPUI drives the UI thread with its own executor. Everything that performs
//! I/O runs on this runtime instead; the UI only awaits results.

use std::future::Future;

use gpui::{App, Global};
use tokio::task::JoinHandle;

/// Worker threads for I/O. Listings and transfers are I/O bound; a few threads
/// keep latency low without competing with rendering.
const WORKER_THREADS: usize = 4;

struct Runtime(tokio::runtime::Runtime);

impl Global for Runtime {}

/// Starts the runtime and registers it as a global.
pub(crate) fn init(cx: &mut App) -> std::io::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKER_THREADS)
        .thread_name("ferge-io")
        .enable_all()
        .build()?;
    cx.set_global(Runtime(runtime));
    Ok(())
}

/// Handle of the runtime, e.g. to enter its context before calling core APIs
/// that spawn tasks.
pub(crate) fn handle(cx: &App) -> tokio::runtime::Handle {
    cx.global::<Runtime>().0.handle().clone()
}

/// Runs `future` on the runtime. The handle can be awaited from the UI executor.
pub(crate) fn spawn<F>(cx: &App, future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    cx.global::<Runtime>().0.spawn(future)
}
