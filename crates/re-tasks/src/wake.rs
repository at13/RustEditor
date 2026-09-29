//! Event-driven wake-ups for results produced on worker threads.
//!
//! Workers send results over `std::sync::mpsc` and then signal a [`Waker`].
//! The UI task awaits [`WakeReceiver::wait`] instead of polling on a timer,
//! so results are applied immediately and an idle editor never wakes up.

use std::sync::mpsc::{self, Receiver, SendError, Sender};

use futures::{StreamExt, channel::mpsc as async_mpsc};

/// Wakes the UI task. Cheap to clone and safe to use from any thread.
#[derive(Clone)]
pub struct Waker(async_mpsc::UnboundedSender<()>);

impl Waker {
    pub fn wake(&self) {
        // The receiver is gone only when the UI is shutting down.
        let _ = self.0.unbounded_send(());
    }

    /// Creates an `mpsc` channel whose sender wakes this waker's receiver.
    pub fn message_channel<T>(&self) -> (WakingSender<T>, Receiver<T>) {
        let (sender, receiver) = mpsc::channel();
        (WakingSender::new(sender, self.clone()), receiver)
    }
}

pub struct WakeReceiver(async_mpsc::UnboundedReceiver<()>);

impl WakeReceiver {
    /// Waits for at least one wake-up and coalesces any others already queued.
    /// Returns `false` once every [`Waker`] has been dropped.
    pub async fn wait(&mut self) -> bool {
        if self.0.next().await.is_none() {
            return false;
        }
        while self.0.try_recv().is_ok() {}
        true
    }
}

pub fn channel() -> (Waker, WakeReceiver) {
    let (sender, receiver) = async_mpsc::unbounded();
    (Waker(sender), WakeReceiver(receiver))
}

/// An `mpsc::Sender` that wakes the UI after every successful send.
pub struct WakingSender<T> {
    sender: Sender<T>,
    waker: Waker,
}

impl<T> WakingSender<T> {
    pub fn new(sender: Sender<T>, waker: Waker) -> Self {
        Self { sender, waker }
    }

    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        self.sender.send(value)?;
        self.waker.wake();
        Ok(())
    }
}

impl<T> Clone for WakingSender<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            waker: self.waker.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    #[test]
    fn send_wakes_the_receiver_and_coalesces_wakeups() {
        let (waker, mut wakes) = channel();
        let (sender, receiver) = waker.message_channel();
        drop(waker);
        let worker = sender.clone();
        thread::spawn(move || {
            worker.send(1).unwrap();
            worker.send(2).unwrap();
        })
        .join()
        .unwrap();

        assert!(futures::executor::block_on(wakes.wait()));
        assert_eq!(receiver.try_iter().collect::<Vec<_>>(), vec![1, 2]);
        // Both wake-ups were coalesced into the single wait above.
        assert!(wakes.0.try_recv().is_err());
    }

    #[test]
    fn wait_returns_false_when_all_wakers_are_dropped() {
        let (waker, mut wakes) = channel();
        drop(waker);
        assert!(!futures::executor::block_on(wakes.wait()));
    }
}
