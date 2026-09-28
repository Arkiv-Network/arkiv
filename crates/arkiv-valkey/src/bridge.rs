//! Calling an async client from a synchronous trait.
//!
//! [`Store`](arkiv_interfaces::store::Store) is synchronous, and deliberately
//! so: the hottest consumer is block execution, which in reth is sync all the
//! way down to revm's `Database`. Making the seam async would push a blocking
//! bridge into that path rather than remove one. `fred` is async-only, so the
//! bridge lives here instead, in one place, where it can be got right once.
//!
//! The obvious implementation — `Runtime::block_on` — panics with "cannot
//! start a runtime from within a runtime" whenever a caller happens to be on
//! an async thread, which makes correctness depend on the call site. Instead
//! the work is **spawned** onto an owned runtime (legal from anywhere) and the
//! calling thread blocks on a plain std channel. That cannot panic on context.
//!
//! It can still *block* a caller's runtime worker if one calls from async
//! without `spawn_blocking`. That is the caller's choice to make and is
//! documented on [`ValkeyStore`](crate::ValkeyStore), rather than a latent
//! panic they cannot see.

use core::future::Future;
use std::sync::mpsc;

use tokio::runtime::{Builder, Runtime};

/// An owned runtime that runs futures to completion for synchronous callers.
#[derive(Debug)]
pub(crate) struct Bridge {
    runtime: Runtime,
}

impl Bridge {
    /// Build a runtime for the client's I/O.
    ///
    /// Two worker threads: the workload is one connection's worth of
    /// round-trips, so this is about having somewhere to drive I/O, not about
    /// parallelism.
    pub(crate) fn new() -> std::io::Result<Self> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("arkiv-valkey")
            .build()?;
        Ok(Self { runtime })
    }

    /// Run `future` to completion, blocking the calling thread.
    ///
    /// Safe to call from anywhere, async context included — see the module
    /// docs for why this spawns rather than `block_on`s.
    pub(crate) fn run<F>(&self, future: F) -> F::Output
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (sender, receiver) = mpsc::sync_channel(1);
        self.runtime.spawn(async move {
            // A send error means the caller gave up waiting; nothing to do.
            let _ = sender.send(future.await);
        });
        receiver
            .recv()
            .expect("the valkey task panicked or the runtime shut down")
    }
}
