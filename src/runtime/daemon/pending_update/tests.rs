use super::*;
use std::cell::Cell;

fn build(version: &str) -> BuildIdentity {
    BuildIdentity {
        version: version.into(),
        commit: "commit".into(),
        target: "test".into(),
        artifact: "artifact".into(),
    }
}

#[test]
fn deferred_attempts_wait_and_eventually_clear_the_request() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(directory.path());
    let spawns = Cell::new(0);
    enqueue(&store, build("5.0.2"), &mut || {
        spawns.set(spawns.get() + 1);
        Ok(())
    })
    .unwrap();
    assert_eq!(spawns.get(), 1);
    let worker = store.worker().unwrap().unwrap();
    record_worker_pid(&worker, 1234).unwrap();
    assert_eq!(
        std::fs::read_to_string(store.0.join("pending-update-worker.lock")).unwrap(),
        "1234\n"
    );
    let mut attempts = [
        Ok(Some(3)),
        Err(io::Error::other("temporary transport failure")),
        Ok(Some(1)),
        Ok(None),
        Ok(Some(0)),
    ]
    .into_iter();
    let waits = Cell::new(0);
    drain(
        &store,
        &worker,
        &mut || attempts.next().unwrap(),
        &mut || {
            let pending = store.read().unwrap().unwrap();
            assert_eq!(
                pending.state,
                if waits.get() == 0 {
                    RetryState::Waiting
                } else {
                    RetryState::Retrying
                }
            );
            waits.set(waits.get() + 1);
        },
    )
    .unwrap();
    assert_eq!(waits.get(), 4);
    assert!(store.read().unwrap().is_none());
    assert!(
        store.worker().unwrap().is_some(),
        "the idle worker gives up its lease"
    );
}

#[test]
fn a_newer_request_survives_an_older_attempts_completion() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(directory.path());
    enqueue(&store, build("5.0.2"), &mut || Ok(())).unwrap();
    let worker = store.worker().unwrap().unwrap();
    let attempts = Cell::new(0);
    drain(
        &store,
        &worker,
        &mut || {
            attempts.set(attempts.get() + 1);
            if attempts.get() == 1 {
                enqueue(&store, build("5.0.3"), &mut || {
                    panic!("a worker already owns the reservation")
                })
                .unwrap();
            } else {
                assert_eq!(store.read().unwrap().unwrap().target.version, "5.0.3");
            }
            Ok(Some(0))
        },
        &mut || panic!("successful attempts do not wait"),
    )
    .unwrap();
    assert_eq!(attempts.get(), 2);
    assert!(store.read().unwrap().is_none());
}

#[test]
fn invalid_or_failed_requests_remain_visible_and_can_be_requeued() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(directory.path());
    assert!(store.read().unwrap().is_none());
    enqueue(&store, build("5.0.2"), &mut || {
        Err(io::Error::other("spawn failed"))
    })
    .unwrap_err();
    assert!(store.read().unwrap().is_some());
    let worker = store.worker().unwrap().unwrap();
    let error = drain(&store, &worker, &mut || Ok(Some(2)), &mut || {
        panic!("unsupported binaries stop retrying")
    })
    .unwrap_err();
    assert!(error.to_string().contains("does not support"));
    assert_eq!(store.read().unwrap().unwrap().state, RetryState::Failed);
    enqueue(&store, build("5.0.3"), &mut || Ok(())).unwrap();
    let mut request = store.read().unwrap().unwrap();
    assert_eq!(request.state, RetryState::Waiting);
    request.schema_version = 2;
    store.write(&request).unwrap();
    assert!(store.read().is_err());
    assert!(
        drain(
            &store,
            &worker,
            &mut || panic!("bad state must not execute an installer"),
            &mut || {}
        )
        .is_err()
    );
    std::fs::write(store.0.join(STATE_FILE), b"invalid json").unwrap();
    assert!(store.read().is_err());
}

#[test]
fn status_reports_the_target_and_automatic_retry_state() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(directory.path());
    let mut out = Vec::new();
    report_pending_update(&mut out, directory.path()).unwrap();
    assert!(out.is_empty());
    enqueue(&store, build("5.0.2"), &mut || Ok(())).unwrap();
    let mut pending = store.read().unwrap().unwrap();
    for (state, expected) in [
        (RetryState::Waiting, "waiting for live"),
        (RetryState::Retrying, "has not completed"),
        (RetryState::Failed, "retry stopped"),
    ] {
        pending.state = state;
        store.write(&pending).unwrap();
        out.clear();
        report_pending_update(&mut out, directory.path()).unwrap();
        let text = String::from_utf8(out.clone()).unwrap();
        assert!(text.contains("v5.0.2"));
        assert!(text.contains(expected));
    }
    std::fs::write(store.0.join(STATE_FILE), b"broken").unwrap();
    out.clear();
    report_pending_update(&mut out, directory.path()).unwrap();
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("state unavailable")
    );
}

#[test]
fn synchronization_reuses_only_the_verified_embedded_installer() {
    assert!(
        synchronization_script(&usagi_cli::cli::commands::update::installer_request(false))
            .unwrap()
            .starts_with(b"#!/bin/bash")
    );
    let invalid = usagi_cli::cli::InstallerRequest::new(b"unverified", [0; 32], false);
    assert!(synchronization_script(&invalid).is_err());
    assert!(worker_lock_acquired(Ok(())).unwrap());
    assert!(!worker_lock_acquired(Err(io::ErrorKind::WouldBlock.into())).unwrap());
    assert_eq!(
        worker_lock_acquired(Err(io::ErrorKind::PermissionDenied.into()))
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
fn the_worker_has_no_terminal_streams_or_original_worktree_dependency() {
    let directory = tempfile::tempdir().unwrap();
    let command = worker_command(Path::new("/installed/usagi"), directory.path());
    assert_eq!(command.get_program(), "/installed/usagi");
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        ["daemon", "continue-update"]
    );
    assert_eq!(command.get_current_dir(), Some(directory.path()));
    assert!(spawn_worker(&directory.path().join("missing"), directory.path()).is_err());
}
