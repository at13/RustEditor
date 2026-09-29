//! Background work for the editor (UI-independent).
//!
//! - [`wake`]: worker results wake the UI task directly (no polling).
//! - [`spawn`]: named worker threads whose panics become errors.
//! - [`CancelToken`]: cooperative cancellation; long jobs check it and stop
//!   early when their result is no longer wanted.
//!
//! Pattern: snapshot the rope, run the job with a token, send the result
//! with the document revision, and apply it only if the revision matches.

pub mod wake;

use std::{
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

/// Shared cancellation flag. Cloning shares the flag.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// The raw flag, for crates that take `Option<&AtomicBool>` and must not
    /// depend on this crate.
    pub fn flag(&self) -> &AtomicBool {
        &self.0
    }
}

/// Keeps the token of the latest job of one kind; starting a new job cancels
/// the previous one.
#[derive(Debug, Default)]
pub struct Latest(Option<CancelToken>);

impl Latest {
    /// Cancels the previous job and returns a token for the next one.
    pub fn restart(&mut self) -> CancelToken {
        if let Some(previous) = self.0.take() {
            previous.cancel();
        }
        let token = CancelToken::new();
        self.0 = Some(token.clone());
        token
    }

    /// Cancels the current job, if any.
    pub fn cancel(&mut self) {
        if let Some(token) = self.0.take() {
            token.cancel();
        }
    }
}

/// Runs `job` on a named thread; a panic is reported as an error result
/// instead of unwinding into the UI. `deliver` receives the result (it is
/// not called if the job was cancelled before finishing).
pub fn spawn<T: Send + 'static>(
    name: impl Into<String>,
    cancel: Option<CancelToken>,
    job: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
    deliver: impl FnOnce(anyhow::Result<T>) + Send + 'static,
) -> io::Result<()> {
    thread::Builder::new().name(name.into()).spawn(move || {
        let result = catch_unwind(AssertUnwindSafe(job))
            .unwrap_or_else(|_| Err(anyhow::anyhow!("background task panicked")));
        if !cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
            deliver(result);
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn spawn_delivers_results_and_turns_panics_into_errors() {
        let (sender, receiver) = mpsc::channel();
        let ok = sender.clone();
        spawn("ok", None, || Ok(42), move |r| ok.send(r.ok()).unwrap()).unwrap();
        assert_eq!(receiver.recv().unwrap(), Some(42));
        spawn(
            "panics",
            None,
            || -> anyhow::Result<i32> { panic!("boom") },
            move |r| sender.send(r.ok()).unwrap(),
        )
        .unwrap();
        assert_eq!(receiver.recv().unwrap(), None);
    }

    #[test]
    fn restarting_cancels_the_previous_job_and_suppresses_its_result() {
        let mut latest = Latest::default();
        let first = latest.restart();
        let second = latest.restart();
        assert!(first.is_cancelled());
        assert!(!second.is_cancelled());
        let (sender, receiver) = mpsc::channel::<i32>();
        let (go, wait) = mpsc::channel::<()>();
        let token = second.clone();
        spawn(
            "cancelled",
            Some(token),
            move || {
                wait.recv().unwrap();
                Ok(1)
            },
            move |r| sender.send(r.unwrap()).unwrap(),
        )
        .unwrap();
        latest.cancel();
        go.send(()).unwrap();
        assert!(receiver.recv().is_err()); // never delivered
        assert!(second.flag().load(Ordering::Relaxed));
    }
}
