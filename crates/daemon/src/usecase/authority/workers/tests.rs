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
