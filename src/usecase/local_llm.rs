//! Provisioning for the optional local LLM the agent offloads work to.
//!
//! usagi delegates light tasks to a local model served through Ollama (see the
//! `usagi llm-mcp` server). This module decides whether the required materials —
//! the `ollama` runtime and the selected model — are present, and installs the
//! missing ones on request. It also makes sure the Ollama *server* is running:
//! a Homebrew-installed `ollama` ships no background service, and the CLI does
//! not auto-start one for `run`/`pull`, so without this every model call fails
//! with "could not connect to ollama server". It never runs on its own: the
//! user opts in via the config screen or `usagi doctor --fix`.
//!
//! All command execution goes through the shared [`CommandRunner`] abstraction
//! (defined alongside `doctor`'s remediation), so the logic here is exercised
//! with a fake runner and never shells out during tests.

use std::time::Duration;

use crate::usecase::doctor::CommandRunner;

/// The Ollama runtime binary usagi drives.
const OLLAMA: &str = "ollama";

/// How [`SetupStep::OllamaInstalled`] labels the install path: the official
/// one-line installer rather than a system package manager.
const INSTALLER: &str = "ollama.com/install.sh";

/// The official Ollama install command, run through a shell so the pipe works.
/// It supports both macOS and Linux and elevates with `sudo` itself where
/// needed (pre-authenticated by [`ensure`] when a password is supplied).
const INSTALL_SCRIPT: &str = "curl -fsSL https://ollama.com/install.sh | sh";

/// Whether the `ollama` runtime is installed and runnable.
pub fn ollama_installed(runner: &dyn CommandRunner) -> bool {
    runner.available(OLLAMA)
}

/// Whether `model` has already been pulled locally.
///
/// Probed with `ollama show <model>`, which exits zero only when the model is
/// present; its output is suppressed since this is a silent capability check.
pub fn model_present(runner: &dyn CommandRunner, model: &str) -> bool {
    runner.check(OLLAMA, &["show", model])
}

/// Whether the Ollama server is currently reachable.
///
/// Probed with `ollama ps`, which connects to the server and exits zero only
/// when it is up. Unlike `run`/`pull`, `ps` never auto-starts the server, so it
/// is a clean liveness check.
pub fn server_running(runner: &dyn CommandRunner) -> bool {
    runner.check(OLLAMA, &["ps"])
}

/// How long [`ensure_server`] waits for a freshly-spawned server to start
/// accepting connections: at most `polls` probes spaced `interval` apart.
#[derive(Debug, Clone, Copy)]
struct ServerWait {
    polls: usize,
    interval: Duration,
}

impl Default for ServerWait {
    fn default() -> Self {
        // ~5s total — comfortably longer than a cold `ollama serve` start
        // (~0.4s locally) without hanging a wedged install indefinitely.
        Self {
            polls: 25,
            interval: Duration::from_millis(200),
        }
    }
}

/// One thing [`ensure`] did (or found already done) while provisioning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupStep {
    /// The `ollama` runtime was already installed.
    OllamaAlreadyPresent,
    /// The `ollama` runtime was installed during this run.
    OllamaInstalled { manager: &'static str },
    /// The Ollama server was already running.
    ServerAlreadyRunning,
    /// The Ollama server was not running and was started during this run.
    ServerStarted,
    /// The model was already pulled.
    ModelAlreadyPresent { model: String },
    /// The model was pulled during this run.
    ModelPulled { model: String },
}

/// Why [`ensure`] could not finish provisioning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupError {
    /// `ollama` is missing and there is no automatic install path on this OS.
    OllamaUnavailable { manual: String },
    /// An `ollama` install was attempted but failed; `manual` says what to do
    /// by hand.
    OllamaInstallFailed {
        manager: &'static str,
        manual: String,
    },
    /// The Ollama server was not running and could not be started (or did not
    /// come up in time after being started).
    ServerStartFailed,
    /// `ollama pull <model>` failed.
    ModelPullFailed { model: String },
}

/// Ensure the local LLM is ready to use for `model`: install `ollama` if it is
/// missing, start its server if it is not already running, then pull the model
/// if it is not already present.
///
/// `sudo` carries the password used to pre-authenticate the privileged steps of
/// the runtime installer non-interactively: `Some(password)` (the TUI install
/// flow) caches sudo credentials up front so nothing prompts mid-install;
/// `None` (the `usagi doctor --fix` CLI) lets the installer prompt on the
/// terminal as usual.
///
/// Returns the ordered list of steps taken on success, or the first error that
/// stopped provisioning. Idempotent — re-running when everything is already in
/// place simply reports the "already present" steps.
pub fn ensure(
    os: &str,
    runner: &dyn CommandRunner,
    model: &str,
    sudo: Option<&str>,
) -> Result<Vec<SetupStep>, SetupError> {
    // The runtime must be installed, then its server brought up, before the
    // model can be pulled — so these run left-to-right and `?` short-circuits
    // on the first failure.
    let ollama = install_ollama(os, runner, sudo)?;
    let server = ensure_server(runner, ServerWait::default())?;
    let model = pull_model(runner, model)?;
    Ok(vec![ollama, server, model])
}

/// Ensure just the `ollama` *runtime* is ready: install it if missing, then
/// start its server. Unlike [`ensure`] this pulls no model — it backs the
/// config screen's "Install" action, which provisions only the runtime so a
/// model can be chosen and pulled separately afterwards. `sudo` carries the
/// password used to pre-authenticate the privileged install steps (see
/// [`ensure`]).
pub fn ensure_runtime(
    os: &str,
    runner: &dyn CommandRunner,
    sudo: Option<&str>,
) -> Result<Vec<SetupStep>, SetupError> {
    let ollama = install_ollama(os, runner, sudo)?;
    let server = ensure_server(runner, ServerWait::default())?;
    Ok(vec![ollama, server])
}

/// Pull `model` into an already-installed runtime, bringing the server up first
/// if it is not running. Backs the config screen's model picker when an
/// uninstalled model is chosen; `ollama pull` is unprivileged, so no `sudo` is
/// needed. Idempotent — a model already present reports as such without
/// re-pulling.
pub fn ensure_model(runner: &dyn CommandRunner, model: &str) -> Result<Vec<SetupStep>, SetupError> {
    let server = ensure_server(runner, ServerWait::default())?;
    let model = pull_model(runner, model)?;
    Ok(vec![server, model])
}

/// Install the `ollama` runtime if it is not already present, using the
/// official installer. `sudo` pre-authenticates the privileged steps when set.
fn install_ollama(
    os: &str,
    runner: &dyn CommandRunner,
    sudo: Option<&str>,
) -> Result<SetupStep, SetupError> {
    if runner.available(OLLAMA) {
        return Ok(SetupStep::OllamaAlreadyPresent);
    }
    // The official installer only supports macOS and Linux; elsewhere there is
    // no automatic path, so point at the manual download instead of guessing.
    if os != "macos" && os != "linux" {
        return Err(SetupError::OllamaUnavailable {
            manual: ollama_manual(),
        });
    }
    let install_failed = || SetupError::OllamaInstallFailed {
        manager: INSTALLER,
        manual: ollama_manual(),
    };
    // Caching sudo credentials first (reading the password from stdin) lets the
    // installer's privileged steps run unattended. A failure here means the
    // password was wrong or sudo is unavailable, so stop before installing.
    if let Some(password) = sudo {
        if !runner
            .run_with_input("sudo", &["-S", "-v"], password)
            .unwrap_or(false)
        {
            return Err(install_failed());
        }
    }
    match runner.run("sh", &["-c", INSTALL_SCRIPT]) {
        Ok(true) => Ok(SetupStep::OllamaInstalled { manager: INSTALLER }),
        Ok(false) | Err(_) => Err(install_failed()),
    }
}

/// Ensure the Ollama server is running, starting it in the background if not.
///
/// A Homebrew-installed `ollama` runs no server on its own, and `run`/`pull`
/// do not auto-start one — so without this every model call fails with "could
/// not connect to ollama server". When the server is down we spawn
/// `ollama serve` detached and poll until it accepts connections, so the model
/// pull that follows (and later `ollama run` calls) do not race the start-up.
fn ensure_server(runner: &dyn CommandRunner, wait: ServerWait) -> Result<SetupStep, SetupError> {
    if server_running(runner) {
        return Ok(SetupStep::ServerAlreadyRunning);
    }
    runner
        .spawn(OLLAMA, &["serve"])
        .map_err(|_| SetupError::ServerStartFailed)?;
    for _ in 0..wait.polls {
        if server_running(runner) {
            return Ok(SetupStep::ServerStarted);
        }
        std::thread::sleep(wait.interval);
    }
    Err(SetupError::ServerStartFailed)
}

/// Pull `model` if it is not already present.
fn pull_model(runner: &dyn CommandRunner, model: &str) -> Result<SetupStep, SetupError> {
    if model_present(runner, model) {
        return Ok(SetupStep::ModelAlreadyPresent {
            model: model.to_string(),
        });
    }
    match runner.run(OLLAMA, &["pull", model]) {
        Ok(true) => Ok(SetupStep::ModelPulled {
            model: model.to_string(),
        }),
        Ok(false) | Err(_) => Err(SetupError::ModelPullFailed {
            model: model.to_string(),
        }),
    }
}

/// Manual install guidance for the `ollama` runtime.
pub fn ollama_manual() -> String {
    "install Ollama from https://ollama.com/download".to_string()
}

/// The message shown when the Ollama server cannot be brought up.
pub fn server_start_failed_message() -> String {
    "could not start the ollama server; try running `ollama serve`".to_string()
}

/// Make sure the Ollama server is running before a model call, starting it in
/// the background if necessary. Used by the MCP backend at request time (where
/// no provisioning step has run), returning a short error message on failure.
pub fn ensure_server_started(runner: &dyn CommandRunner) -> Result<(), String> {
    ensure_server(runner, ServerWait::default())
        .map(|_| ())
        .map_err(|_| server_start_failed_message())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A configurable [`CommandRunner`] that records the commands it ran, so a
    /// test can assert both the outcome and that the right commands fired.
    struct FakeRunner {
        available: Vec<&'static str>,
        /// Models the fake reports as already pulled (matched on `show <model>`).
        present_models: Vec<&'static str>,
        /// Result returned by `run` (install / pull).
        run: std::io::Result<bool>,
        /// Result returned by the sudo pre-authentication (`run_with_input`).
        sudo: std::io::Result<bool>,
        ran: RefCell<Vec<String>>,
        /// The input piped to the last `run_with_input` call (the password).
        piped: RefCell<Option<String>>,
        /// Number of `ollama ps` probes that report the server DOWN before it
        /// comes UP. `0` means the server is up from the first probe.
        server_down_for: usize,
        ps_probes: RefCell<usize>,
        /// Result returned by `spawn` (starting `ollama serve`).
        spawn: std::io::Result<()>,
        spawned: RefCell<Vec<String>>,
    }

    impl FakeRunner {
        fn new(available: Vec<&'static str>, run: std::io::Result<bool>) -> Self {
            Self {
                available,
                present_models: Vec::new(),
                run,
                sudo: Ok(true),
                ran: RefCell::new(Vec::new()),
                piped: RefCell::new(None),
                // By default the server is already up, so `ensure` tests focused
                // on install/pull need not opt into the start path.
                server_down_for: 0,
                ps_probes: RefCell::new(0),
                spawn: Ok(()),
                spawned: RefCell::new(Vec::new()),
            }
        }

        fn with_present_models(mut self, models: Vec<&'static str>) -> Self {
            self.present_models = models;
            self
        }

        fn with_sudo(mut self, sudo: std::io::Result<bool>) -> Self {
            self.sudo = sudo;
            self
        }

        /// Report the server DOWN for the first `n` `ps` probes, then UP.
        fn with_server_down_for(mut self, n: usize) -> Self {
            self.server_down_for = n;
            self
        }

        /// Make `spawn` (starting the server) fail.
        fn with_spawn_error(mut self) -> Self {
            self.spawn = Err(std::io::Error::other("spawn failed"));
            self
        }
    }

    impl CommandRunner for FakeRunner {
        fn available(&self, program: &str) -> bool {
            self.available.contains(&program)
        }

        fn run(&self, program: &str, args: &[&str]) -> std::io::Result<bool> {
            self.ran
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            match &self.run {
                Ok(ok) => Ok(*ok),
                Err(e) => Err(std::io::Error::new(e.kind(), e.to_string())),
            }
        }

        fn run_with_input(
            &self,
            program: &str,
            args: &[&str],
            input: &str,
        ) -> std::io::Result<bool> {
            self.ran
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            *self.piped.borrow_mut() = Some(input.to_string());
            match &self.sudo {
                Ok(ok) => Ok(*ok),
                Err(e) => Err(std::io::Error::new(e.kind(), e.to_string())),
            }
        }

        fn check(&self, _program: &str, args: &[&str]) -> bool {
            if args.first() == Some(&"ps") {
                // Mimics `ollama ps`: DOWN for the first `server_down_for`
                // probes, UP thereafter.
                let mut probes = self.ps_probes.borrow_mut();
                *probes += 1;
                return *probes > self.server_down_for;
            }
            // Mimics `ollama show <model>`: succeeds only for known models.
            args.last()
                .is_some_and(|model| self.present_models.contains(model))
        }

        fn spawn(&self, program: &str, args: &[&str]) -> std::io::Result<()> {
            self.spawned
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            match &self.spawn {
                Ok(()) => Ok(()),
                Err(e) => Err(std::io::Error::new(e.kind(), e.to_string())),
            }
        }
    }

    #[test]
    fn ollama_installed_reflects_availability() {
        assert!(ollama_installed(&FakeRunner::new(vec!["ollama"], Ok(true))));
        assert!(!ollama_installed(&FakeRunner::new(vec![], Ok(true))));
    }

    #[test]
    fn model_present_uses_the_show_probe() {
        let runner =
            FakeRunner::new(vec!["ollama"], Ok(true)).with_present_models(vec!["qwen2.5-coder:7b"]);
        assert!(model_present(&runner, "qwen2.5-coder:7b"));
        assert!(!model_present(&runner, "qwen2.5:7b"));
    }

    #[test]
    fn ensure_is_a_no_op_when_everything_is_present() {
        // ollama installed, server already up, model already pulled.
        let runner =
            FakeRunner::new(vec!["ollama"], Ok(true)).with_present_models(vec!["qwen2.5-coder:7b"]);
        let steps = ensure("macos", &runner, "qwen2.5-coder:7b", None).unwrap();
        assert_eq!(
            steps,
            vec![
                SetupStep::OllamaAlreadyPresent,
                SetupStep::ServerAlreadyRunning,
                SetupStep::ModelAlreadyPresent {
                    model: "qwen2.5-coder:7b".to_string()
                }
            ]
        );
        // Nothing was installed, started, or pulled.
        assert!(runner.ran.borrow().is_empty());
        assert!(runner.spawned.borrow().is_empty());
    }

    #[test]
    fn ensure_installs_ollama_starts_the_server_and_pulls_the_model() {
        // ollama absent, no sudo password (the CLI path): the official installer
        // runs directly; the server is down until started; the model is pulled.
        let runner = FakeRunner::new(vec![], Ok(true)).with_server_down_for(1);
        let steps = ensure("linux", &runner, "qwen2.5:7b", None).unwrap();
        assert_eq!(
            steps,
            vec![
                SetupStep::OllamaInstalled { manager: INSTALLER },
                SetupStep::ServerStarted,
                SetupStep::ModelPulled {
                    model: "qwen2.5:7b".to_string()
                }
            ]
        );
        assert_eq!(
            *runner.ran.borrow(),
            vec![
                format!("sh -c {INSTALL_SCRIPT}"),
                "ollama pull qwen2.5:7b".to_string(),
            ]
        );
        // No sudo pre-authentication without a password; the server was started
        // in the background before the pull.
        assert!(runner.piped.borrow().is_none());
        assert_eq!(*runner.spawned.borrow(), vec!["ollama serve"]);
    }

    #[test]
    fn ensure_preauthenticates_sudo_when_a_password_is_supplied() {
        // The TUI path hands over a password: sudo is validated first (reading
        // the password from stdin), then the installer and pull run.
        let runner = FakeRunner::new(vec![], Ok(true));
        let steps = ensure("macos", &runner, "qwen2.5:7b", Some("hunter2")).unwrap();
        assert_eq!(steps[0], SetupStep::OllamaInstalled { manager: INSTALLER });
        assert_eq!(
            *runner.ran.borrow(),
            vec![
                "sudo -S -v".to_string(),
                format!("sh -c {INSTALL_SCRIPT}"),
                "ollama pull qwen2.5:7b".to_string(),
            ]
        );
        assert_eq!(runner.piped.borrow().as_deref(), Some("hunter2"));
    }

    #[test]
    fn ensure_reports_when_the_os_has_no_installer() {
        // The official installer supports only macOS and Linux.
        let runner = FakeRunner::new(vec![], Ok(true));
        assert_eq!(
            ensure("windows", &runner, "qwen2.5:7b", None),
            Err(SetupError::OllamaUnavailable {
                manual: ollama_manual()
            })
        );
        // Nothing was attempted on an unsupported OS.
        assert!(runner.ran.borrow().is_empty());
    }

    #[test]
    fn ensure_stops_when_sudo_preauthentication_fails() {
        // A wrong password (sudo exits non-zero) aborts before the installer.
        let runner = FakeRunner::new(vec![], Ok(true)).with_sudo(Ok(false));
        assert_eq!(
            ensure("linux", &runner, "qwen2.5:7b", Some("wrong")),
            Err(SetupError::OllamaInstallFailed {
                manager: INSTALLER,
                manual: ollama_manual()
            })
        );
        // The installer never ran; only the failed sudo validation did.
        assert_eq!(*runner.ran.borrow(), vec!["sudo -S -v".to_string()]);
    }

    #[test]
    fn ensure_stops_when_sudo_validation_errors() {
        // sudo itself fails to spawn (an I/O error rather than a clean non-zero
        // exit): treated like a failed install, and the installer never runs.
        let runner =
            FakeRunner::new(vec![], Ok(true)).with_sudo(Err(std::io::Error::other("no sudo")));
        assert_eq!(
            ensure("linux", &runner, "qwen2.5:7b", Some("pw")),
            Err(SetupError::OllamaInstallFailed {
                manager: INSTALLER,
                manual: ollama_manual()
            })
        );
        assert_eq!(*runner.ran.borrow(), vec!["sudo -S -v".to_string()]);
    }

    #[test]
    fn ensure_reports_a_failed_ollama_install() {
        // The installer script exits non-zero.
        let runner = FakeRunner::new(vec![], Ok(false));
        assert_eq!(
            ensure("macos", &runner, "qwen2.5:7b", None),
            Err(SetupError::OllamaInstallFailed {
                manager: INSTALLER,
                manual: ollama_manual()
            })
        );
    }

    #[test]
    fn ensure_reports_a_failed_model_pull() {
        // ollama present, model missing, and the pull fails (spawn error here).
        let runner = FakeRunner::new(vec!["ollama"], Err(std::io::Error::other("boom")));
        assert_eq!(
            ensure("macos", &runner, "qwen2.5:7b", None),
            Err(SetupError::ModelPullFailed {
                model: "qwen2.5:7b".to_string()
            })
        );
    }

    #[test]
    fn ollama_manual_points_at_the_official_download() {
        assert!(ollama_manual().contains("ollama.com"));
    }

    // --- server start-up ---------------------------------------------------

    /// A near-instant wait so the start-up tests never sleep for real.
    fn fast_wait(polls: usize) -> ServerWait {
        ServerWait {
            polls,
            interval: Duration::ZERO,
        }
    }

    #[test]
    fn server_running_uses_the_ps_probe() {
        let up = FakeRunner::new(vec!["ollama"], Ok(true));
        assert!(server_running(&up));

        let down = FakeRunner::new(vec!["ollama"], Ok(true)).with_server_down_for(1);
        assert!(!server_running(&down));
    }

    #[test]
    fn ensure_server_reports_an_already_running_server() {
        let runner = FakeRunner::new(vec!["ollama"], Ok(true));
        assert_eq!(
            ensure_server(&runner, ServerWait::default()),
            Ok(SetupStep::ServerAlreadyRunning)
        );
        // A running server is never (re)started.
        assert!(runner.spawned.borrow().is_empty());
    }

    #[test]
    fn ensure_server_starts_it_and_waits_until_it_is_ready() {
        // Down for the initial probe and one more poll, then up: this exercises
        // both the spawn and the poll/sleep loop.
        let runner = FakeRunner::new(vec!["ollama"], Ok(true)).with_server_down_for(2);
        assert_eq!(
            ensure_server(&runner, fast_wait(5)),
            Ok(SetupStep::ServerStarted)
        );
        assert_eq!(*runner.spawned.borrow(), vec!["ollama serve"]);
    }

    #[test]
    fn ensure_server_reports_a_failed_spawn() {
        // Server down and `ollama serve` cannot be launched at all.
        let runner = FakeRunner::new(vec!["ollama"], Ok(true))
            .with_server_down_for(usize::MAX)
            .with_spawn_error();
        assert_eq!(
            ensure_server(&runner, fast_wait(3)),
            Err(SetupError::ServerStartFailed)
        );
    }

    #[test]
    fn ensure_server_times_out_when_the_server_never_comes_up() {
        // Spawn succeeds but the server never accepts connections; after the
        // bounded poll budget is spent, give up with ServerStartFailed.
        let runner = FakeRunner::new(vec!["ollama"], Ok(true)).with_server_down_for(usize::MAX);
        assert_eq!(
            ensure_server(&runner, fast_wait(3)),
            Err(SetupError::ServerStartFailed)
        );
        assert_eq!(*runner.spawned.borrow(), vec!["ollama serve"]);
    }

    #[test]
    fn ensure_server_started_is_ok_when_the_server_is_running() {
        let runner = FakeRunner::new(vec!["ollama"], Ok(true));
        assert_eq!(ensure_server_started(&runner), Ok(()));
    }

    #[test]
    fn ensure_server_started_reports_a_short_message_on_failure() {
        // Server down and the spawn fails -> the caller gets the guidance text.
        let runner = FakeRunner::new(vec!["ollama"], Ok(true))
            .with_server_down_for(usize::MAX)
            .with_spawn_error();
        assert_eq!(
            ensure_server_started(&runner),
            Err(server_start_failed_message())
        );
    }

    #[test]
    fn server_start_failed_message_mentions_ollama_serve() {
        assert!(server_start_failed_message().contains("ollama serve"));
    }

    // --- runtime-only / model-only provisioning ----------------------------

    #[test]
    fn ensure_runtime_installs_ollama_and_starts_the_server_without_a_model() {
        // ollama absent and the server down: the runtime is installed and the
        // server started, but no model is pulled (unlike `ensure`).
        let runner = FakeRunner::new(vec![], Ok(true)).with_server_down_for(1);
        let steps = ensure_runtime("linux", &runner, None).unwrap();
        assert_eq!(
            steps,
            vec![
                SetupStep::OllamaInstalled { manager: INSTALLER },
                SetupStep::ServerStarted,
            ]
        );
        // The installer and server spawn ran; nothing was pulled.
        assert_eq!(
            *runner.ran.borrow(),
            vec![format!("sh -c {INSTALL_SCRIPT}")]
        );
        assert_eq!(*runner.spawned.borrow(), vec!["ollama serve"]);
    }

    #[test]
    fn ensure_runtime_is_a_no_op_when_already_installed_and_running() {
        let runner = FakeRunner::new(vec!["ollama"], Ok(true));
        assert_eq!(
            ensure_runtime("macos", &runner, None),
            Ok(vec![
                SetupStep::OllamaAlreadyPresent,
                SetupStep::ServerAlreadyRunning,
            ])
        );
        assert!(runner.ran.borrow().is_empty());
        assert!(runner.spawned.borrow().is_empty());
    }

    #[test]
    fn ensure_runtime_reports_an_unsupported_os() {
        let runner = FakeRunner::new(vec![], Ok(true));
        assert_eq!(
            ensure_runtime("windows", &runner, None),
            Err(SetupError::OllamaUnavailable {
                manual: ollama_manual()
            })
        );
    }

    #[test]
    fn ensure_model_starts_the_server_and_pulls_the_model() {
        // Runtime present, server down once, model not yet pulled: the server is
        // started, then the model pulled. No runtime install runs.
        let runner = FakeRunner::new(vec!["ollama"], Ok(true)).with_server_down_for(1);
        let steps = ensure_model(&runner, "qwen2.5-coder:3b").unwrap();
        assert_eq!(
            steps,
            vec![
                SetupStep::ServerStarted,
                SetupStep::ModelPulled {
                    model: "qwen2.5-coder:3b".to_string()
                }
            ]
        );
        assert_eq!(
            *runner.ran.borrow(),
            vec!["ollama pull qwen2.5-coder:3b".to_string()]
        );
        assert_eq!(*runner.spawned.borrow(), vec!["ollama serve"]);
    }

    #[test]
    fn ensure_model_is_a_no_op_when_the_model_is_already_present() {
        let runner =
            FakeRunner::new(vec!["ollama"], Ok(true)).with_present_models(vec!["qwen2.5-coder:7b"]);
        assert_eq!(
            ensure_model(&runner, "qwen2.5-coder:7b"),
            Ok(vec![
                SetupStep::ServerAlreadyRunning,
                SetupStep::ModelAlreadyPresent {
                    model: "qwen2.5-coder:7b".to_string()
                }
            ])
        );
        // Nothing was pulled.
        assert!(runner.ran.borrow().is_empty());
    }

    #[test]
    fn ensure_model_reports_a_failed_pull() {
        // Server up, model absent, and `ollama pull` fails.
        let runner = FakeRunner::new(vec!["ollama"], Ok(false));
        assert_eq!(
            ensure_model(&runner, "qwen2.5:7b"),
            Err(SetupError::ModelPullFailed {
                model: "qwen2.5:7b".to_string()
            })
        );
    }

    #[test]
    fn ensure_model_propagates_a_server_start_failure() {
        // The server is down and cannot be started, so the pull never runs.
        let runner = FakeRunner::new(vec!["ollama"], Ok(true))
            .with_server_down_for(usize::MAX)
            .with_spawn_error();
        assert_eq!(
            ensure_model(&runner, "qwen2.5:7b"),
            Err(SetupError::ServerStartFailed)
        );
        assert!(runner.ran.borrow().is_empty());
    }
}
