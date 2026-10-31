//! Delivery on a thread of its own, so an import reads its next window while the last one's turns are
//! being sent. One delivery pass runs at a time; each [`Courier::nudge`] asks for one more.

use anyhow::Result;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use super::sync::Pass;

/// What a delivery pass came to: what it did, or why it couldn't run (another delivery held the queue,
/// the server's address couldn't be read, ...).
pub type Outcome = Result<Pass, String>;

pub struct Courier {
    nudges: Option<Sender<()>>,
    outcomes: Receiver<Outcome>,
    worker: Option<JoinHandle<()>>,
    /// Passes asked for whose outcome hasn't been collected yet.
    in_flight: usize,
}

impl Courier {
    /// Starts the thread; `deliver` runs one pass ([`super::sync::deliver_paced`] outside tests).
    pub fn start(mut deliver: impl FnMut() -> Result<Pass> + Send + 'static) -> Self {
        let (nudges, requests) = mpsc::channel::<()>();
        let (results, outcomes) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            for () in requests {
                let outcome = deliver().map_err(|error| format!("{error:#}"));
                if results.send(outcome).is_err() {
                    break;
                }
            }
        });
        Self {
            nudges: Some(nudges),
            outcomes,
            worker: Some(worker),
            in_flight: 0,
        }
    }

    /// Asks for a delivery pass after any already asked for.
    pub fn nudge(&mut self) {
        if self
            .nudges
            .as_ref()
            .is_some_and(|nudges| nudges.send(()).is_ok())
        {
            self.in_flight += 1;
        }
    }

    /// The outcomes of the passes finished since the last call, without waiting.
    pub fn finished(&mut self) -> Vec<Outcome> {
        let outcomes: Vec<Outcome> = self.outcomes.try_iter().collect();
        self.in_flight -= outcomes.len();
        outcomes
    }

    /// Waits up to `timeout` for the next pass to finish.
    pub fn wait(&mut self, timeout: std::time::Duration) -> Option<Outcome> {
        let outcome = self.outcomes.recv_timeout(timeout).ok()?;
        self.in_flight -= 1;
        Some(outcome)
    }

    /// No pass is asked for or running.
    pub fn idle(&self) -> bool {
        self.in_flight == 0
    }
}

impl Drop for Courier {
    /// Lets the pass in progress finish (a half-delivered batch would only be sent again), then stops.
    fn drop(&mut self) {
        self.nudges.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn each_nudge_runs_one_pass_off_the_calling_thread() {
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = runs.clone();
        let caller = std::thread::current().id();
        let mut courier = Courier::start(move || {
            assert_ne!(std::thread::current().id(), caller);
            Ok(Pass {
                delivered: counted.fetch_add(1, Ordering::SeqCst) + 1,
                ..Pass::default()
            })
        });

        courier.nudge();
        courier.nudge();
        assert!(!courier.idle());
        let first = courier.wait(Duration::from_secs(5)).unwrap().unwrap();
        let second = courier.wait(Duration::from_secs(5)).unwrap().unwrap();

        assert_eq!((first.delivered, second.delivered), (1, 2));
        assert!(courier.idle());
        assert!(courier.finished().is_empty());
    }

    #[test]
    fn a_failed_pass_is_an_outcome_not_a_stop() {
        let mut courier = Courier::start(|| anyhow::bail!("queue busy"));

        courier.nudge();
        assert_eq!(
            courier.wait(Duration::from_secs(5)),
            Some(Err("queue busy".into()))
        );
        courier.nudge();
        assert!(courier.wait(Duration::from_secs(5)).is_some());
    }
}
