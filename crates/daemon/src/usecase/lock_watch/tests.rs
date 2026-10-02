use super::*;

fn probe(name: &'static str, waiting_since: Option<Instant>) -> Arc<LockProbe> {
    let probe = Arc::new(LockProbe::new(name));
    probe.publish(waiting_since);
    probe
}

#[test]
fn a_weak_lock_is_acquired_while_its_owner_lives_and_reports_when_it_is_gone() {
    let owner = Arc::new(Mutex::new(1));
    let target = Arc::downgrade(&owner);
    assert!(target.acquire());

    let poisoned = Arc::clone(&owner);
    let _ = std::thread::spawn(move || {
        let _held = poisoned.lock().unwrap();
        panic!("poison the probed lock");
    })
    .join();
    assert!(owner.is_poisoned());
    assert!(target.acquire(), "a poisoned lock is still available");

    drop(owner);
    assert!(!target.acquire());
}

#[test]
fn measure_publishes_the_start_only_while_the_acquisition_runs() {
    let probe = LockProbe::new("agent runtime");
    assert_eq!(probe.waiting_since(), None);

    let inside = probe.measure(|| probe.waiting_since());

    assert!(inside.is_some());
    assert_eq!(probe.waiting_since(), None);
}

#[test]
fn a_stall_is_reported_once_and_its_recovery_once() {
    let start = Instant::now();
    let stalled = probe("agent runtime", Some(start));
    let mut watch = LockWatch::new(Duration::from_secs(10), [Arc::clone(&stalled)]);

    assert!(watch.observe(start + Duration::from_secs(9)).is_empty());
    assert_eq!(
        watch.observe(start + Duration::from_secs(10)),
        vec![LockWatchEvent::Stalled {
            lock: "agent runtime",
            waited: Duration::from_secs(10),
        }]
    );
    assert!(watch.observe(start + Duration::from_secs(30)).is_empty());

    stalled.publish(None);
    assert_eq!(
        watch.observe(start + Duration::from_secs(40)),
        vec![LockWatchEvent::Recovered {
            lock: "agent runtime",
            stalled_for: Duration::from_secs(40),
        }]
    );
    assert!(watch.observe(start + Duration::from_secs(50)).is_empty());
}

#[test]
fn a_new_wait_after_a_stall_recovers_the_old_one_and_is_judged_on_its_own() {
    let start = Instant::now();
    let stalled = probe("terminal runtime", Some(start));
    let idle = probe("agent runtime", None);
    let mut watch = LockWatch::new(Duration::from_secs(10), [Arc::clone(&stalled), idle]);
    assert_eq!(watch.observe(start + Duration::from_secs(10)).len(), 1);

    let second = start + Duration::from_secs(20);
    stalled.publish(Some(second));
    assert_eq!(
        watch.observe(second + Duration::from_secs(10)),
        vec![
            LockWatchEvent::Recovered {
                lock: "terminal runtime",
                stalled_for: Duration::from_secs(30),
            },
            LockWatchEvent::Stalled {
                lock: "terminal runtime",
                waited: Duration::from_secs(10),
            },
        ]
    );
}

#[test]
fn lock_events_name_the_lock_and_the_duration() {
    assert_eq!(
        LockWatchEvent::Stalled {
            lock: "agent runtime",
            waited: Duration::from_millis(12_500),
        }
        .to_string(),
        "daemon lock `agent runtime` has not been available for 12s; requests that need it are parked and the daemon may be deadlocked"
    );
    assert_eq!(
        LockWatchEvent::Recovered {
            lock: "agent runtime",
            stalled_for: Duration::from_secs(14),
        }
        .to_string(),
        "daemon lock `agent runtime` became available again after about 14s"
    );
}

#[test]
fn capacity_is_reported_at_three_quarters_and_cleared_at_half() {
    let mut watch = CapacityWatch::new(256);
    let mut reports = Vec::new();
    for outstanding in [10, 191, 192, 250, 129, 128, 100, 192] {
        watch.observe(outstanding, |line| reports.push(line.to_owned()));
    }

    assert_eq!(
        reports,
        vec![
            "daemon client workers reached 192/256 of capacity; new connections are refused at the limit",
            "daemon client workers are back to 128/256 of capacity",
            "daemon client workers reached 192/256 of capacity; new connections are refused at the limit",
        ]
    );
}

#[test]
fn the_shipping_threshold_outlasts_every_bounded_lock_holder() {
    let timing = LockWatchTiming::SHIPPING;
    assert!(timing.threshold > Duration::from_secs(2));
    assert!(timing.watch_tick < timing.threshold);
    assert!(timing.probe_tick < timing.threshold);
}

#[test]
fn a_probe_stops_when_its_owner_is_gone_or_shutdown_was_requested() {
    let probe = LockProbe::new("agent runtime");
    let gone: Weak<Mutex<()>> = Weak::new();
    run_lock_probe(
        &probe,
        &gone,
        &ShutdownRequest::new(),
        Duration::from_secs(60),
    );
    assert_eq!(probe.waiting_since(), None);

    let owner = Arc::new(Mutex::new(()));
    let requested = ShutdownRequest::new();
    requested.request();
    run_lock_probe(
        &probe,
        &Arc::downgrade(&owner),
        &requested,
        Duration::from_secs(60),
    );
}

fn wait_for(reports: &Mutex<Vec<String>>, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !reports
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.contains(needle))
    {
        assert!(
            Instant::now() < deadline,
            "no report containing {needle:?}: {:?}",
            reports.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn the_watchdog_reports_a_held_lock_and_its_release_then_stops_on_shutdown() {
    let owner = Arc::new(Mutex::new(()));
    let shutdown = Arc::new(ShutdownRequest::new());
    let reports = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&reports);
    let held = owner.lock().unwrap();

    let threads = start_lock_watch(
        vec![LockTarget::new("agent runtime", Arc::downgrade(&owner))],
        Arc::new(ClientWorkers::new()),
        &shutdown,
        LockWatchTiming {
            probe_tick: Duration::from_millis(5),
            watch_tick: Duration::from_millis(5),
            threshold: Duration::from_millis(50),
        },
        Arc::new(move |line: &str| sink.lock().unwrap().push(line.to_owned())),
    )
    .unwrap();

    wait_for(&reports, "has not been available");
    assert!(
        reports.lock().unwrap()[0].ends_with("(0 client workers outstanding)"),
        "{:?}",
        reports.lock().unwrap()
    );
    drop(held);
    wait_for(&reports, "became available again");

    shutdown.request();
    threads.watch.join().unwrap();
    for probe in threads.probes {
        probe.join().unwrap();
    }
}
