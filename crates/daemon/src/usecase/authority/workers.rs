//! Retiring the client workers a generation still holds.
//!
//! Closing the accept loop stops *new* connections; it does nothing for a
//! worker parked in a blocking frame read on a connection that is already open.
//! Waiting on a count would not help either — a count reaching zero says the
//! workers decided to stop, not that they finished.
//!
//! So a generation retains both halves of every client worker: a handle that
//! can shut the stream down (unblocking the read) and the thread's
//! [`JoinHandle`]. Retirement shuts every stream down first, then joins every
//! thread, and only then may the endpoint and the process be reclaimed.

use std::io;
use std::sync::Mutex;
use std::thread::JoinHandle;

/// The half of a client connection that can unblock a parked reader.
///
/// The real implementation calls `shutdown(2)` on the accepted stream; tests
/// inject a fake that closes whatever the fake worker is parked on.
pub trait ConnectionShutdown: Send + Sync {
    /// Unblock any frame read parked on this connection.
    ///
    /// # Errors
    /// Returns an error when the connection cannot be shut down.
    fn shutdown(&self) -> io::Result<()>;
}

struct ClientWorker {
    connection: Box<dyn ConnectionShutdown>,
    handle: JoinHandle<()>,
}

#[derive(Default)]
struct WorkerSet {
    entries: Vec<ClientWorker>,
    retired: bool,
}

/// What retirement observed. It is reported rather than swallowed: a worker
/// that could not be unblocked is exactly the thing that must not be mistaken
/// for a clean collection.
#[derive(Debug, Default)]
pub struct RetireReport {
    /// Threads that were joined.
    pub joined: usize,
    /// Threads whose join observed a panic.
    pub panicked: usize,
    /// Connections that could not be shut down. Their workers are still
    /// joined — the shutdown failure is reported, not used as a reason to
    /// abandon a thread.
    pub shutdown_failures: Vec<io::Error>,
}

impl RetireReport {
    /// Whether every worker was unblocked and joined without a panic.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.panicked == 0 && self.shutdown_failures.is_empty()
    }
}

/// Every client worker this generation is responsible for joining.
#[derive(Default)]
pub struct ClientWorkers {
    state: Mutex<WorkerSet>,
}

impl ClientWorkers {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Retain a worker together with the handle that can unblock it.
    ///
    /// Before retirement the returned report is empty. After the set is sealed,
    /// this call retires the late worker synchronously and returns that report,
    /// so an accept-loop race cannot leave a worker past endpoint cleanup.
    pub fn register(
        &self,
        connection: Box<dyn ConnectionShutdown>,
        handle: JoinHandle<()>,
    ) -> RetireReport {
        let worker = ClientWorker { connection, handle };
        let mut state = self.lock();
        if !state.retired {
            state.entries.push(worker);
            return RetireReport::default();
        }
        // The accept loop can already have accepted and spawned a connection
        // worker when collection seals the set. It still calls `register`
        // before the accept-loop thread can finish; joining it synchronously
        // here means the composition root's accept-loop join remains a barrier
        // for every worker, including this last race.
        drop(state);
        retire_workers(vec![worker])
    }

    /// How many workers are retained.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.lock().entries.len()
    }

    /// Join the workers that have already finished, leaving the live ones alone.
    ///
    /// A generation serves connections for as long as it lives, so a set that
    /// only ever grew would retain one thread handle per *historical* connection
    /// — an unbounded cost paid by exactly the long-lived daemon this authority
    /// exists to replace. Only a worker [`JoinHandle::is_finished`] reports
    /// complete is taken, so this never blocks and never shuts a live connection
    /// down: it is the ordinary path, and [`retire`](Self::retire) stays the only
    /// thing that unblocks a parked reader.
    ///
    /// The report counts what was joined here rather than at retirement, which is
    /// what keeps "every worker was joined exactly once" true across the two.
    pub fn reap_finished(&self) -> RetireReport {
        // Partitioned under the lock and joined after it is released: a finished
        // thread cannot block its own join, but holding the lock across the joins
        // would still serialize every concurrent registration behind them.
        let finished = {
            let mut state = self.lock();
            let mut finished = Vec::new();
            let mut live = Vec::with_capacity(state.entries.len());
            for worker in std::mem::take(&mut state.entries) {
                if worker.handle.is_finished() {
                    // The shutdown half goes with it. Nothing is parked on a
                    // connection whose worker has already returned.
                    finished.push(worker.handle);
                } else {
                    live.push(worker);
                }
            }
            state.entries = live;
            finished
        };
        let mut report = RetireReport::default();
        for handle in finished {
            if handle.join().is_err() {
                report.panicked += 1;
            }
            report.joined += 1;
        }
        report
    }

    /// Shut every retained connection down, then join every retained worker.
    ///
    /// The workers are taken out of the set before any of them is joined, so a
    /// worker that registers while retiring cannot deadlock against this call.
    /// The set is sealed before the lock is released: a registration that races
    /// after that point shuts down and joins its own worker synchronously instead
    /// of escaping the retirement barrier. A second retirement is a no-op.
    pub fn retire(&self) -> RetireReport {
        let workers = {
            let mut state = self.lock();
            state.retired = true;
            std::mem::take(&mut state.entries)
        };
        retire_workers(workers)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, WorkerSet> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn retire_workers(workers: Vec<ClientWorker>) -> RetireReport {
    let mut report = RetireReport::default();
    let mut handles = Vec::with_capacity(workers.len());
    for worker in workers {
        if let Err(error) = worker.connection.shutdown() {
            report.shutdown_failures.push(error);
        }
        handles.push(worker.handle);
    }
    for handle in handles {
        if handle.join().is_err() {
            report.panicked += 1;
        }
        report.joined += 1;
    }
    report
}

#[cfg(test)]
mod tests;
