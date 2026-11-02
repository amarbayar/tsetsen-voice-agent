//! Racing work against a request's cancellation and deadline.

use std::future::Future;
use std::time::Duration;

use tsetsen_core::{Ctx, Interrupted};

/// How often cancellation is checked while waiting. Barge-in feels instant
/// below about 50 ms; checking a flag every 10 ms costs nothing.
const POLL: Duration = Duration::from_millis(10);

/// Runs `work` until it finishes or `cx` is cancelled or out of time.
/// Dropping `work` early must be safe, as it is for async I/O.
pub(crate) async fn cancellable<F: Future>(cx: &Ctx, work: F) -> Result<F::Output, Interrupted> {
    cx.check()?;
    tokio::pin!(work);
    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            output = &mut work => return Ok(output),
            _ = tick.tick() => cx.check()?,
        }
    }
}
