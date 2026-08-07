//! Bounded secret resolution and the owned 1Password CLI child lifecycle.
//!
//! The domain owns every count limit. This adapter validates before it starts a
//! worker, uses a bounded job queue with a fixed worker count, and funnels the
//! ordered outcomes through [`collect`](crate::usecase::env::collect).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use crate::domain::settings::{
    EnvBindings, EnvLimitError, MAX_CONCURRENT_SECRET_READS, is_secret_reference, valid_bindings,
    validate_env_limits,
};
use crate::usecase::env::{ResolvedEnvironment, SecretResolver, collect};

const OP_TIMEOUT: Duration = Duration::from_secs(30);
const OP_TERMINATE_GRACE: Duration = Duration::from_secs(2);
const OP_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// The 1Password CLI as a [`SecretResolver`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpCli;

fn op_read_command(reference: &str, service_account_token: Option<&str>) -> Command {
    let mut command = Command::new("op");
    command
        .arg("read")
        .arg("--no-newline")
        .arg(reference)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(token) = service_account_token {
        command.env("OP_SERVICE_ACCOUNT_TOKEN", token);
    }
    command
}

impl SecretResolver for OpCli {
    #[coverage(off)] // coverage: reason=real_io owner=core expires=2027-01-31 tests=successful_child_is_reaped_and_its_readers_are_joined
    fn read(&self, reference: &str) -> Result<String, String> {
        real::run(reference, None)
    }

    #[coverage(off)] // coverage: reason=real_io owner=core expires=2027-01-31 tests=service_account_token_is_scoped_to_the_owned_op_child
    fn read_with_service_account_token(
        &self,
        reference: &str,
        service_account_token: Option<&str>,
    ) -> Result<String, String> {
        real::run(reference, service_account_token)
    }
}

/// Resolve references with bounded fan-out while preserving binding order.
///
/// # Errors
/// Returns the domain limit error before any resolver call is made.
pub fn resolve_parallel<R>(
    bindings: &EnvBindings,
    resolver: &R,
) -> Result<ResolvedEnvironment, EnvLimitError>
where
    R: SecretResolver + Sync,
{
    resolve_parallel_with_service_account_token(bindings, resolver, None)
}

/// Resolve references with bounded fan-out and an optional daemon-owned
/// 1Password service-account credential.
///
/// # Errors
/// Returns the domain limit error before any resolver call is made.
pub fn resolve_parallel_with_service_account_token<R>(
    bindings: &EnvBindings,
    resolver: &R,
    service_account_token: Option<&str>,
) -> Result<ResolvedEnvironment, EnvLimitError>
where
    R: SecretResolver + Sync,
{
    validate_env_limits(bindings)?;
    let requested: Vec<(String, String)> = valid_bindings(bindings)
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    let secret_count = requested
        .iter()
        .filter(|(_, value)| is_secret_reference(value))
        .count();
    if secret_count == 0 {
        return Ok(collect(
            requested
                .into_iter()
                .map(|(name, value)| (name, value.clone(), Ok(value))),
        ));
    }

    let outcomes = requested
        .iter()
        .map(|(_, value)| {
            if is_secret_reference(value) {
                Err("secret worker unavailable".to_owned())
            } else {
                Ok(value.clone())
            }
        })
        .collect::<Vec<_>>();
    let outcomes = Arc::new(Mutex::new(outcomes));
    std::thread::scope(|scope| {
        let worker_count = secret_count.min(MAX_CONCURRENT_SECRET_READS);
        let (jobs_tx, jobs_rx) = mpsc::sync_channel::<(usize, String)>(worker_count);
        let jobs_rx = Arc::new(Mutex::new(jobs_rx));
        for _ in 0..worker_count {
            let jobs = Arc::clone(&jobs_rx);
            let outcomes = Arc::clone(&outcomes);
            scope.spawn(move || {
                loop {
                    let job = jobs
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .recv();
                    let Ok((index, reference)) = job else { break };
                    let outcome = catch_unwind(AssertUnwindSafe(|| {
                        resolver.read_with_service_account_token(&reference, service_account_token)
                    }))
                    .unwrap_or_else(|_| Err("secret read thread panicked".to_owned()));
                    outcomes
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)[index] = outcome;
                }
            });
        }
        for (index, (_, value)) in requested.iter().enumerate() {
            if is_secret_reference(value) {
                let _ = jobs_tx.send((index, value.clone()));
            }
        }
        drop(jobs_tx);
    });
    let outcomes = outcomes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    Ok(collect(requested.into_iter().zip(outcomes).map(
        |((name, reference), outcome)| (name, reference, outcome),
    )))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChildExit {
    success: bool,
    display: String,
}

trait OwnedChild {
    fn try_wait(&mut self) -> Result<Option<ChildExit>, String>;
    fn terminate(&mut self) -> Result<(), String>;
    fn kill(&mut self) -> Result<(), String>;
    fn wait(&mut self) -> Result<ChildExit, String>;
    fn join_output(&mut self) -> Result<(Vec<u8>, Vec<u8>), String>;
}

trait ChildRunner {
    fn spawn(&self, reference: &str) -> Result<Box<dyn OwnedChild>, String>;
}

trait Time {
    fn elapsed(&self) -> Duration;
    fn sleep(&self, duration: Duration);
}

trait Cancellation {
    fn is_cancelled(&self) -> bool;
}

fn run_owned_child(
    runner: &dyn ChildRunner,
    time: &dyn Time,
    reference: &str,
    cancellation: &dyn Cancellation,
    timeout: Duration,
    terminate_grace: Duration,
    poll_interval: Duration,
) -> Result<String, String> {
    let mut child = runner.spawn(reference)?;
    let deadline = time.elapsed().saturating_add(timeout);
    let mut status = None;
    while time.elapsed() < deadline && !cancellation.is_cancelled() {
        match child.try_wait() {
            Ok(Some(exit)) => {
                status = Some(exit);
                break;
            }
            Ok(None) => time.sleep(poll_interval),
            Err(_) => break,
        }
    }

    let interrupted = status.is_none();
    if interrupted {
        let _ = child.terminate();
        let grace_deadline = time.elapsed().saturating_add(terminate_grace);
        while time.elapsed() < grace_deadline {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    status = Some(exit);
                    break;
                }
                Ok(None) => time.sleep(poll_interval),
                Err(_) => break,
            }
        }
        if status.is_none() {
            let _ = child.kill();
        }
    }

    // `wait` is called even after `try_wait` observed exit: it is the explicit
    // reap fence, and implementations must make repeated wait return the status.
    let reaped = child.wait();
    let output = child.join_output();
    if cancellation.is_cancelled() {
        return Err("op read was cancelled".to_owned());
    }
    if interrupted {
        return Err(format!("op exceeded its {timeout:?} deadline"));
    }
    let exit = reaped?;
    let (stdout, stderr) = output?;
    if !exit.success {
        let detail = String::from_utf8_lossy(&stderr).trim().to_owned();
        return Err(format!(
            "op exited with {}: {}",
            exit.display,
            if detail.is_empty() {
                "no stderr"
            } else {
                &detail
            }
        ));
    }
    Ok(String::from_utf8_lossy(&stdout)
        .trim_end_matches(['\n', '\r'])
        .to_owned())
}

mod real {
    #![coverage(off)] // coverage: reason=real_io owner=core expires=2027-01-31 tests=owned_child_timeout_escalates_and_reaps_before_joining_output

    use std::io::Read;
    use std::process::Child;
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use super::{
        Cancellation, ChildExit, ChildRunner, OP_POLL_INTERVAL, OP_TERMINATE_GRACE, OP_TIMEOUT,
        OwnedChild, Time, op_read_command, run_owned_child,
    };

    pub(super) fn run(
        reference: &str,
        service_account_token: Option<&str>,
    ) -> Result<String, String> {
        let outcome = run_owned_child(
            &SystemRunner {
                service_account_token,
            },
            &SystemTime::new(),
            reference,
            &NeverCancelled,
            OP_TIMEOUT,
            OP_TERMINATE_GRACE,
            OP_POLL_INTERVAL,
        );
        outcome.map_err(|error| match service_account_token {
            Some(token) => error.replace(token, "[redacted]"),
            None => error,
        })
    }

    struct SystemRunner<'a> {
        service_account_token: Option<&'a str>,
    }

    impl ChildRunner for SystemRunner<'_> {
        fn spawn(&self, reference: &str) -> Result<Box<dyn OwnedChild>, String> {
            let mut child = op_read_command(reference, self.service_account_token)
                .spawn()
                .map_err(|_| "failed to start secret resolver".to_owned())?;
            let stdout = reader(child.stdout.take().expect("piped stdout"));
            let stderr = reader(child.stderr.take().expect("piped stderr"));
            Ok(Box::new(SystemChild {
                child,
                stdout: Some(stdout),
                stderr: Some(stderr),
            }))
        }
    }

    fn reader(mut pipe: impl Read + Send + 'static) -> JoinHandle<std::io::Result<Vec<u8>>> {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes)?;
            Ok(bytes)
        })
    }

    struct SystemChild {
        child: Child,
        stdout: Option<JoinHandle<std::io::Result<Vec<u8>>>>,
        stderr: Option<JoinHandle<std::io::Result<Vec<u8>>>>,
    }

    impl SystemChild {
        fn exit(status: std::process::ExitStatus) -> ChildExit {
            ChildExit {
                success: status.success(),
                display: status.to_string(),
            }
        }
    }

    impl OwnedChild for SystemChild {
        fn try_wait(&mut self) -> Result<Option<ChildExit>, String> {
            self.child
                .try_wait()
                .map(|status| status.map(Self::exit))
                .map_err(|_| "could not observe secret resolver".to_owned())
        }

        fn terminate(&mut self) -> Result<(), String> {
            #[cfg(unix)]
            {
                let pid = libc::pid_t::try_from(self.child.id())
                    .map_err(|_| "could not terminate secret resolver".to_owned())?;
                // The PID comes directly from this still-owned Child handle.
                let result = unsafe { libc::kill(pid, libc::SIGTERM) };
                (result == 0)
                    .then_some(())
                    .ok_or_else(|| "could not terminate secret resolver".to_owned())
            }
            #[cfg(not(unix))]
            {
                self.child
                    .kill()
                    .map_err(|_| "could not terminate secret resolver".to_owned())
            }
        }

        fn kill(&mut self) -> Result<(), String> {
            self.child
                .kill()
                .map_err(|_| "could not kill secret resolver".to_owned())
        }

        fn wait(&mut self) -> Result<ChildExit, String> {
            self.child
                .wait()
                .map(Self::exit)
                .map_err(|_| "could not reap secret resolver".to_owned())
        }

        fn join_output(&mut self) -> Result<(Vec<u8>, Vec<u8>), String> {
            fn join(
                handle: &mut Option<JoinHandle<std::io::Result<Vec<u8>>>>,
            ) -> Result<Vec<u8>, String> {
                handle
                    .take()
                    .ok_or_else(|| "secret resolver output already joined".to_owned())?
                    .join()
                    .map_err(|_| "secret resolver output reader panicked".to_owned())?
                    .map_err(|_| "could not read secret resolver output".to_owned())
            }
            let stdout = join(&mut self.stdout)?;
            let stderr = join(&mut self.stderr)?;
            Ok((stdout, stderr))
        }
    }

    struct SystemTime(Instant);

    struct NeverCancelled;

    impl Cancellation for NeverCancelled {
        fn is_cancelled(&self) -> bool {
            false
        }
    }

    impl SystemTime {
        fn new() -> Self {
            Self(Instant::now())
        }
    }

    impl Time for SystemTime {
        fn elapsed(&self) -> Duration {
            self.0.elapsed()
        }

        fn sleep(&self, duration: Duration) {
            std::thread::sleep(duration);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeResolver {
        reads: Mutex<Vec<String>>,
    }

    impl SecretResolver for FakeResolver {
        fn read(&self, reference: &str) -> Result<String, String> {
            self.reads.lock().unwrap().push(reference.to_owned());
            if reference.contains("Locked") {
                Err("op is locked".to_owned())
            } else {
                Ok(format!("secret:{reference}"))
            }
        }
    }

    struct PanickingResolver;

    impl SecretResolver for PanickingResolver {
        fn read(&self, _reference: &str) -> Result<String, String> {
            panic!("resolver thread died");
        }
    }

    struct ServiceAccountResolver {
        tokens: Mutex<Vec<Option<String>>>,
    }

    impl SecretResolver for ServiceAccountResolver {
        fn read(&self, reference: &str) -> Result<String, String> {
            Ok(reference.to_owned())
        }

        fn read_with_service_account_token(
            &self,
            reference: &str,
            service_account_token: Option<&str>,
        ) -> Result<String, String> {
            self.tokens
                .lock()
                .unwrap()
                .push(service_account_token.map(str::to_owned));
            Ok(format!("resolved:{reference}"))
        }
    }

    fn bindings(pairs: &[(&str, &str)]) -> EnvBindings {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn bounded_parallel_resolution_keeps_literals_success_failures_and_order() {
        let reader = FakeResolver {
            reads: Mutex::new(Vec::new()),
        };
        let resolved = resolve_parallel(
            &bindings(&[
                ("RUST_LOG", "debug"),
                ("OPEN", "op://Private/Open/token"),
                ("LOCKED", "op://Private/Locked/token"),
                ("1BAD", "op://Private/Bad/token"),
            ]),
            &reader,
        )
        .unwrap();

        assert_eq!(
            resolved.values,
            BTreeMap::from([
                (
                    "OPEN".to_owned(),
                    "secret:op://Private/Open/token".to_owned()
                ),
                ("RUST_LOG".to_owned(), "debug".to_owned()),
            ])
        );
        assert_eq!(resolved.failures.len(), 1);
        assert_eq!(resolved.failures[0].name, "LOCKED");
    }

    #[test]
    fn a_panicking_reader_fails_only_its_own_binding() {
        let resolved = resolve_parallel(
            &bindings(&[("SECRET", "op://Private/Item/field"), ("LITERAL", "kept")]),
            &PanickingResolver,
        )
        .unwrap();
        assert_eq!(resolved.values["LITERAL"], "kept");
        assert_eq!(resolved.failures[0].error, "secret read thread panicked");
    }

    #[test]
    fn an_over_limit_request_has_zero_resolver_effects() {
        let reader = FakeResolver {
            reads: Mutex::new(Vec::new()),
        };
        let oversized = (0..=crate::domain::settings::MAX_SECRET_REFERENCES)
            .map(|index| (format!("SECRET_{index}"), format!("op://vault/{index}")))
            .collect();
        assert_eq!(
            resolve_parallel(&oversized, &reader),
            Err(EnvLimitError::TooManySecretReferences)
        );
        assert!(reader.reads.lock().unwrap().is_empty());
    }

    #[test]
    fn literal_only_bindings_do_not_start_secret_workers() {
        let reader = FakeResolver {
            reads: Mutex::new(Vec::new()),
        };
        assert_eq!(
            resolve_parallel(&bindings(&[("RUST_LOG", "debug")]), &reader)
                .unwrap()
                .values,
            BTreeMap::from([("RUST_LOG".to_owned(), "debug".to_owned())])
        );
        assert!(reader.reads.lock().unwrap().is_empty());
    }

    #[test]
    fn service_account_token_reaches_only_secret_resolver_calls() {
        let resolver = ServiceAccountResolver {
            tokens: Mutex::new(Vec::new()),
        };
        assert_eq!(resolver.read("literal"), Ok("literal".to_owned()));
        let environment = resolve_parallel_with_service_account_token(
            &bindings(&[
                ("GH_TOKEN", "op://Private/GitHub/token"),
                ("LITERAL", "kept"),
            ]),
            &resolver,
            Some("service-token"),
        )
        .unwrap();

        assert_eq!(
            environment.values["GH_TOKEN"],
            "resolved:op://Private/GitHub/token"
        );
        assert_eq!(environment.values["LITERAL"], "kept");
        assert_eq!(
            resolver.tokens.lock().unwrap().as_slice(),
            [Some("service-token".to_owned())]
        );
    }

    #[test]
    fn service_account_token_is_scoped_to_the_owned_op_child() {
        let command = op_read_command("op://Private/GitHub/token", Some("service-token"));
        let configured = command
            .get_envs()
            .find(|(name, _)| *name == "OP_SERVICE_ACCOUNT_TOKEN")
            .and_then(|(_, value)| value)
            .and_then(|value| value.to_str());
        assert_eq!(configured, Some("service-token"));

        assert!(
            op_read_command("op://Private/GitHub/token", None)
                .get_envs()
                .all(|(name, _)| name != "OP_SERVICE_ACCOUNT_TOKEN")
        );
    }

    struct ConcurrencyResolver {
        active: AtomicUsize,
        maximum: AtomicUsize,
        first_wave: std::sync::Barrier,
        calls: AtomicUsize,
    }

    impl SecretResolver for ConcurrencyResolver {
        fn read(&self, reference: &str) -> Result<String, String> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.maximum.fetch_max(active, Ordering::SeqCst);
            if self.calls.fetch_add(1, Ordering::SeqCst) < MAX_CONCURRENT_SECRET_READS {
                self.first_wave.wait();
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(reference.to_owned())
        }
    }

    #[test]
    fn fan_out_never_exceeds_the_domain_concurrency_limit() {
        let resolver = ConcurrencyResolver {
            active: AtomicUsize::new(0),
            maximum: AtomicUsize::new(0),
            first_wave: std::sync::Barrier::new(MAX_CONCURRENT_SECRET_READS),
            calls: AtomicUsize::new(0),
        };
        let bindings = (0..(MAX_CONCURRENT_SECRET_READS + 3))
            .map(|index| (format!("SECRET_{index}"), format!("op://vault/{index}")))
            .collect();
        resolve_parallel(&bindings, &resolver).unwrap();
        assert_eq!(
            resolver.maximum.load(Ordering::SeqCst),
            MAX_CONCURRENT_SECRET_READS
        );
    }

    #[derive(Default)]
    struct FakeTime(Mutex<Duration>);

    impl Time for FakeTime {
        fn elapsed(&self) -> Duration {
            *self.0.lock().unwrap()
        }

        fn sleep(&self, duration: Duration) {
            *self.0.lock().unwrap() += duration;
        }
    }

    struct NeverCancelled;

    impl Cancellation for NeverCancelled {
        fn is_cancelled(&self) -> bool {
            false
        }
    }

    struct AlwaysCancelled;

    impl Cancellation for AlwaysCancelled {
        fn is_cancelled(&self) -> bool {
            true
        }
    }

    struct FakeRunner {
        child: Mutex<Option<FakeChild>>,
        spawn_error: Option<String>,
    }

    impl ChildRunner for FakeRunner {
        fn spawn(&self, _reference: &str) -> Result<Box<dyn OwnedChild>, String> {
            if let Some(error) = &self.spawn_error {
                return Err(error.clone());
            }
            Ok(Box::new(self.child.lock().unwrap().take().unwrap()))
        }
    }

    struct FakeChild {
        events: Arc<Mutex<Vec<&'static str>>>,
        ready: bool,
        try_wait_errors: usize,
        exit_after_terminate: bool,
        terminated: bool,
        exit: ChildExit,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    }

    impl OwnedChild for FakeChild {
        fn try_wait(&mut self) -> Result<Option<ChildExit>, String> {
            self.events.lock().unwrap().push("try_wait");
            if self.try_wait_errors > 0 {
                self.try_wait_errors -= 1;
                return Err("observation failed".to_owned());
            }
            Ok(
                (self.ready || (self.terminated && self.exit_after_terminate))
                    .then(|| self.exit.clone()),
            )
        }

        fn terminate(&mut self) -> Result<(), String> {
            self.events.lock().unwrap().push("terminate");
            self.terminated = true;
            Ok(())
        }

        fn kill(&mut self) -> Result<(), String> {
            self.events.lock().unwrap().push("kill");
            Ok(())
        }

        fn wait(&mut self) -> Result<ChildExit, String> {
            self.events.lock().unwrap().push("wait");
            Ok(self.exit.clone())
        }

        fn join_output(&mut self) -> Result<(Vec<u8>, Vec<u8>), String> {
            self.events.lock().unwrap().push("join_output");
            Ok((self.stdout.clone(), self.stderr.clone()))
        }
    }

    fn fake_runner(exit_after_terminate: bool) -> (FakeRunner, Arc<Mutex<Vec<&'static str>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            FakeRunner {
                child: Mutex::new(Some(FakeChild {
                    events: Arc::clone(&events),
                    ready: false,
                    try_wait_errors: 0,
                    exit_after_terminate,
                    terminated: false,
                    exit: ChildExit {
                        success: false,
                        display: "signal".to_owned(),
                    },
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })),
                spawn_error: None,
            },
            events,
        )
    }

    #[test]
    fn owned_child_timeout_escalates_and_reaps_before_joining_output() {
        let (runner, events) = fake_runner(false);
        let error = run_owned_child(
            &runner,
            &FakeTime::default(),
            "op://vault/item",
            &NeverCancelled,
            Duration::from_millis(20),
            Duration::from_millis(20),
            Duration::from_millis(10),
        )
        .unwrap_err();
        assert!(error.contains("deadline"));
        let events = events.lock().unwrap();
        let terminate = events
            .iter()
            .position(|event| *event == "terminate")
            .unwrap();
        let kill = events.iter().position(|event| *event == "kill").unwrap();
        let wait = events.iter().position(|event| *event == "wait").unwrap();
        let join = events
            .iter()
            .position(|event| *event == "join_output")
            .unwrap();
        assert!(terminate < kill && kill < wait && wait < join);
    }

    #[test]
    fn successful_child_is_reaped_and_its_readers_are_joined() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let runner = FakeRunner {
            child: Mutex::new(Some(FakeChild {
                events: Arc::clone(&events),
                ready: true,
                try_wait_errors: 0,
                exit_after_terminate: false,
                terminated: false,
                exit: ChildExit {
                    success: true,
                    display: "exit status: 0".to_owned(),
                },
                stdout: b"secret\n".to_vec(),
                stderr: Vec::new(),
            })),
            spawn_error: None,
        };
        assert_eq!(
            run_owned_child(
                &runner,
                &FakeTime::default(),
                "op://vault/item",
                &NeverCancelled,
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_millis(1),
            ),
            Ok("secret".to_owned())
        );
        assert_eq!(
            events.lock().unwrap().as_slice(),
            ["try_wait", "wait", "join_output"]
        );
    }

    #[test]
    fn graceful_timeout_exit_is_reaped_and_does_not_get_killed() {
        let (runner, events) = fake_runner(true);
        run_owned_child(
            &runner,
            &FakeTime::default(),
            "op://vault/item",
            &NeverCancelled,
            Duration::ZERO,
            Duration::from_millis(20),
            Duration::from_millis(10),
        )
        .unwrap_err();
        let events = events.lock().unwrap();
        assert!(events.contains(&"terminate"));
        assert!(!events.contains(&"kill"));
        assert_eq!(&events[events.len() - 2..], ["wait", "join_output"]);
    }

    #[test]
    fn observation_errors_still_escalate_reap_and_join() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let runner = FakeRunner {
            child: Mutex::new(Some(FakeChild {
                events: Arc::clone(&events),
                ready: false,
                try_wait_errors: 2,
                exit_after_terminate: false,
                terminated: false,
                exit: ChildExit {
                    success: false,
                    display: "signal".to_owned(),
                },
                stdout: Vec::new(),
                stderr: Vec::new(),
            })),
            spawn_error: None,
        };
        run_owned_child(
            &runner,
            &FakeTime::default(),
            "op://vault/item",
            &NeverCancelled,
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_millis(1),
        )
        .unwrap_err();
        assert_eq!(
            events.lock().unwrap().as_slice(),
            [
                "try_wait",
                "terminate",
                "try_wait",
                "kill",
                "wait",
                "join_output"
            ]
        );
    }

    #[test]
    fn nonzero_child_reports_empty_and_present_stderr_safely() {
        for (stderr, suffix) in [
            (Vec::new(), "no stderr"),
            (b"vault is locked\n".to_vec(), "vault is locked"),
        ] {
            let runner = FakeRunner {
                child: Mutex::new(Some(FakeChild {
                    events: Arc::new(Mutex::new(Vec::new())),
                    ready: true,
                    try_wait_errors: 0,
                    exit_after_terminate: false,
                    terminated: false,
                    exit: ChildExit {
                        success: false,
                        display: "exit status: 1".to_owned(),
                    },
                    stdout: Vec::new(),
                    stderr,
                })),
                spawn_error: None,
            };
            let error = run_owned_child(
                &runner,
                &FakeTime::default(),
                "op://vault/item",
                &NeverCancelled,
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_millis(1),
            )
            .unwrap_err();
            assert!(error.ends_with(suffix), "{error}");
        }
    }

    #[test]
    fn spawn_failure_has_no_child_cleanup_effect() {
        let runner = FakeRunner {
            child: Mutex::new(None),
            spawn_error: Some("failed to start secret resolver".to_owned()),
        };
        assert_eq!(
            run_owned_child(
                &runner,
                &FakeTime::default(),
                "op://vault/item",
                &NeverCancelled,
                Duration::ZERO,
                Duration::ZERO,
                Duration::from_millis(1),
            ),
            Err("failed to start secret resolver".to_owned())
        );
    }

    #[test]
    fn cancellation_uses_the_same_owned_child_cleanup_path() {
        let (runner, events) = fake_runner(false);
        assert_eq!(
            run_owned_child(
                &runner,
                &FakeTime::default(),
                "op://vault/item",
                &AlwaysCancelled,
                Duration::from_secs(30),
                Duration::ZERO,
                Duration::from_millis(1),
            ),
            Err("op read was cancelled".to_owned())
        );
        let events = events.lock().unwrap();
        assert_eq!(
            events.as_slice(),
            ["terminate", "kill", "wait", "join_output"]
        );
    }
}
