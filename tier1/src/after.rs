//! Post-response deferred work collector.
//!
//! The pipeline may schedule cache writes and other side-effects that should
//! not block the response to the client.  Entry points collect these futures
//! via [`AfterResponse`] and drain them after the response is sent, using the
//! platform's post-response mechanism:
//!
//! - **Native (tokio)**: `tokio::spawn` - tasks are independent of the handler
//!   and run after it returns.
//! - **Cloudflare Workers**: `ctx.wait_until(future)` - the Workers runtime
//!   keeps the isolate alive until all registered futures settle.
//!
//! The `ThumbCook` does not hold an `AfterResponse` directly - pipeline steps
//! return or schedule work through it explicitly.  Entry points pass it into
//! the pipeline where needed and drain it at the end.
//!
//! # Example - native
//!
//! ```rust,ignore
//! let mut after = AfterResponse::new();
//! let (result, trace) = ThumbCook::new(spec).run().await;
//! // cache write scheduled during run() lands here:
//! after.drain_spawn();   // fires all tasks onto tokio thread pool
//! // response already being sent concurrently by axum
//! ```
//!
//! # Example - Workers
//!
//! ```rust,ignore
//! let mut after = AfterResponse::new();
//! let (result, trace) = ThumbCook::new(spec).run().await;
//! for task in after.drain() {
//!     ctx.wait_until(task);
//! }
//! ```

use std::future::Future;
use std::pin::Pin;

/// A fire-and-forget future.
#[cfg(feature = "native")]
pub type DeferredFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// A fire-and-forget future for single-threaded wasm targets.
#[cfg(not(feature = "native"))]
pub type DeferredFuture = Pin<Box<dyn Future<Output = ()> + 'static>>;

/// Collects futures that should run after the response is sent.
#[derive(Default)]
pub struct AfterResponse {
    tasks: Vec<DeferredFuture>,
}

impl AfterResponse {
    pub fn new() -> Self {
        Self::default()
    }

    /// Schedule a future to run after the response.
    #[cfg(feature = "native")]
    pub fn push(&mut self, fut: impl Future<Output = ()> + Send + 'static) {
        self.tasks.push(Box::pin(fut));
    }

    /// Schedule a future to run after the response.
    #[cfg(not(feature = "native"))]
    pub fn push(&mut self, fut: impl Future<Output = ()> + 'static) {
        self.tasks.push(Box::pin(fut));
    }

    /// Drain all scheduled futures, returning them to the caller.
    ///
    /// Cloudflare Workers entry points pass each one to `ctx.wait_until()`.
    pub fn drain(&mut self) -> impl Iterator<Item = DeferredFuture> + '_ {
        self.tasks.drain(..)
    }

    /// Drain and spawn all tasks onto the tokio thread pool.
    ///
    /// Native entry points call this after the response future is handed to
    /// axum.  Each task runs independently; failures are logged but do not
    /// affect the response.
    #[cfg(feature = "native")]
    pub fn drain_spawn(&mut self) {
        for task in self.tasks.drain(..) {
            tokio::task::spawn(task);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }
}
