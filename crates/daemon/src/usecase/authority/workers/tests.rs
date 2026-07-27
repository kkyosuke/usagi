use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use super::*;

/// A connection whose "parked read" is a channel receive; shutting it down
/// closes the sender so the read returns, exactly as `shutdown(2)` unblocks a
/// blocking `recv` on a socket.
struct FakeConnection {
    sender: std::sync::Mutex<Option<Sender<()>>>,
    failure: Option<&'static str>,
}

impl FakeConnection {
    fn new(failure: Option<&'static str>) -> (Arc<Self>, Receiver<()>) {
        let (sender, receiver) = channel();
        (
            Arc::new(Self {
                sender: std::sync::Mutex::new(Some(sender)),
                failure,
            }),
            receiver,
        )
    }
}

impl ConnectionShutdown for Arc<FakeConnection> {
    fn shutdown(&self) -> io::Result<()> {
        // The stream is closed even when the syscall reports a failure, so the
        // worker is still joinable afterwards.
        drop(self.sender.lock().unwrap().take());
        self.failure
            .map_or(Ok(()), |message| Err(io::Error::other(message)))
    }
}

#[test]
fn retirement_unblocks_every_stream_before_joining_its_worker() {
    let workers = ClientWorkers::new();
    assert_eq!(workers.outstanding(), 0);

    let mut parked = Vec::new();
    for _ in 0..3 {
        let (connection, receiver) = FakeConnection::new(None);
        let served = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = {
            let served = Arc::clone(&served);
            std::thread::spawn(move || {
                // Parked in a blocking frame read until the stream is shut down.
                assert!(receiver.recv().is_err());
                served.store(true, std::sync::atomic::Ordering::Release);
            })
        };
        workers.register(Box::new(connection), handle);
        parked.push(served);
    }
    assert_eq!(workers.outstanding(), 3);

    let report = workers.retire();
    assert!(report.is_clean());
    assert_eq!(report.joined, 3);
    assert_eq!(report.panicked, 0);
    // Joined, not merely counted: each worker actually ran to completion.
    for served in parked {
        assert!(served.load(std::sync::atomic::Ordering::Acquire));
    }
    // A second retirement is a no-op rather than a double join.
    assert_eq!(workers.outstanding(), 0);
    assert_eq!(workers.retire().joined, 0);
}

#[test]
fn a_shutdown_failure_is_reported_and_the_worker_is_still_joined() {
    let workers = ClientWorkers::default();
    let (connection, receiver) = FakeConnection::new(Some("shutdown refused"));
    let handle = std::thread::spawn(move || {
        assert!(receiver.recv().is_err());
    });
    workers.register(Box::new(connection), handle);

    let report = workers.retire();
    assert!(!report.is_clean());
    assert_eq!(report.joined, 1);
    assert_eq!(report.shutdown_failures.len(), 1);
    assert_eq!(report.shutdown_failures[0].to_string(), "shutdown refused");
}

/// Reaping is the ordinary path a serving generation takes so its retained set
/// tracks live connections rather than historical ones. It joins what has already
/// finished, leaves what is still parked, and never shuts a live stream down —
/// that distinction is the whole reason it is not just `retire`.
#[test]
fn reaping_joins_the_finished_workers_and_leaves_the_parked_ones_connected() {
    let workers = ClientWorkers::new();

    let (done_connection, done_receiver) = FakeConnection::new(None);
    let done = std::thread::spawn(move || drop(done_receiver));
    // Registered only once the thread has returned, so `is_finished` is settled
    // rather than raced on.
    while !done.is_finished() {
        std::thread::yield_now();
    }
    workers.register(Box::new(done_connection), done);

    let (live_connection, live_receiver) = FakeConnection::new(None);
    let live = Arc::clone(&live_connection);
    let parked = std::thread::spawn(move || {
        assert!(live_receiver.recv().is_err());
    });
    workers.register(Box::new(live_connection), parked);
    assert_eq!(workers.outstanding(), 2);

    let reaped = workers.reap_finished();
    assert!(reaped.is_clean());
    assert_eq!(reaped.joined, 1);
    // The parked worker is still retained *and* still connected: reaping did not
    // reach for its shutdown half.
    assert_eq!(workers.outstanding(), 1);
    assert!(live.sender.lock().unwrap().is_some());

    // A second reap finds nothing new, and retirement still collects the live one.
    assert_eq!(workers.reap_finished().joined, 0);
    assert_eq!(workers.retire().joined, 1);
    assert_eq!(workers.outstanding(), 0);
}

/// A worker that panicked before it was reaped is reported by the reap, not
/// silently dropped: "joined exactly once" has to hold across both paths.
#[test]
fn reaping_reports_a_worker_that_panicked_before_it_was_collected() {
    let workers = ClientWorkers::new();
    let (connection, receiver) = FakeConnection::new(None);
    let handle = std::thread::spawn(move || {
        drop(receiver);
        panic!("client worker died");
    });
    while !handle.is_finished() {
        std::thread::yield_now();
    }
    workers.register(Box::new(connection), handle);

    let report = workers.reap_finished();
    assert!(!report.is_clean());
    assert_eq!(report.joined, 1);
    assert_eq!(report.panicked, 1);
    assert_eq!(workers.outstanding(), 0);
}

#[test]
fn a_panicking_worker_is_reported_rather_than_hidden() {
    let workers = ClientWorkers::new();
    let (connection, receiver) = FakeConnection::new(None);
    let handle = std::thread::spawn(move || {
        assert!(receiver.recv().is_err());
        panic!("client worker died");
    });
    workers.register(Box::new(connection), handle);

    let report = workers.retire();
    assert!(!report.is_clean());
    assert_eq!(report.joined, 1);
    assert_eq!(report.panicked, 1);
    assert!(format!("{report:?}").contains("panicked"));
}
