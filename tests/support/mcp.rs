//! Production-process MCP test support.
//!
//! Tests using this module talk to the shipping `usagi mcp` binary over its
//! stdio JSON-RPC interface. The MCP process autostarts the shipping daemon;
//! both global data and the git workspace are isolated per harness.

#![cfg(unix)]

use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use usagi_core::domain::id::{OperationId, SessionId, WorkspaceId};
use usagi_core::domain::{agent::AgentProfileId, settings::Settings};
use usagi_core::infrastructure::ipc::ClientWorkspace;
use usagi_core::infrastructure::store::workspace::Storage;
use usagi_core::usecase::client::{
    AgentLaunchIntent, ClientPolicy, DaemonClient, DaemonReply, DaemonRequest, IpcClient,
    SessionAction,
};
use usagi_daemon::infrastructure::unix_transport::{connect_current, ensure_private_dir_all};

use super::daemon::{Channel, HeavyE2eLock, heavy_e2e_lock, reap, usagi_command};

/// Claude は必ず OS sandbox launcher の中で起動するため、`bwrap` を持たない Linux CI では
/// fail-closed で起動が拒否される。この debug ビルド専用 seam は launcher と `--settings` フックの
/// live 配線をそのまま通したまま拘束だけを外し、E2E を platform 非依存にする
/// （[`usagi_core::usecase::claude_sandbox::passthrough_requested`]）。
const SANDBOX_PASSTHROUGH: &str =
    usagi_core::usecase::claude_sandbox::PASSTHROUGH_ENVIRONMENT_VARIABLE;

fn shipping_build_identity() -> usagi_core::infrastructure::ipc::BuildIdentity {
    usagi_core::infrastructure::ipc::build_identity(
        env!("CARGO_PKG_VERSION"),
        env!("USAGI_BUILD_COMMIT"),
        env!("USAGI_BUILD_TARGET"),
        env!("USAGI_BUILD_PROFILE"),
        env!("USAGI_BUILD_SOURCE_ID"),
    )
}

pub struct McpHarness {
    workspace: tempfile::TempDir,
    cwd: PathBuf,
    home: tempfile::TempDir,
    /// この harness が起動する usagi の runtime channel。data directory の割り付けと
    /// `USAGI_RUNTIME_MODE` の両方がここから決まる。
    channel: Channel,
    fixture_bin: PathBuf,
    fixture_log: PathBuf,
    fixture_argv: PathBuf,
    fixture_mcp_input: PathBuf,
    fixture_mcp_output: PathBuf,
    process: McpProcess,
    _heavy_e2e: HeavyE2eLock,
}

#[derive(Clone)]
pub struct FixtureArgv {
    pub runtime: String,
    pub arguments: Vec<String>,
}

struct McpProcess {
    child: Option<Child>,
    stdin: Box<dyn Write>,
    stdout: Box<dyn BufRead>,
    next_id: u64,
}

impl McpHarness {
    #[must_use]
    pub fn start() -> Self {
        Self::start_at(Channel::Local, None, false, None, false)
    }

    /// production channel（`USAGI_RUNTIME_MODE=production`）で起動する。
    ///
    /// production は `$USAGI_HOME` 自体を selected directory にする唯一の mode なので、
    /// 「base と selected の関係」を base 側から間違えても local では露見しない。
    #[must_use]
    pub fn start_in_production() -> Self {
        Self::start_at(Channel::Production, None, false, None, false)
    }

    /// Every shipping Agent grammar, including Sakana AI's `codex-fugu`.
    #[must_use]
    pub fn start_with_all_agents() -> Self {
        Self::start_at(Channel::Local, None, false, None, true)
    }

    #[must_use]
    pub fn start_in_session(name: &str) -> Self {
        Self::start_at(Channel::Local, Some(name), false, None, false)
    }

    #[must_use]
    pub fn start_with_tool_availability(issue: bool, memory: bool) -> Self {
        Self::start_at(Channel::Local, None, false, Some((issue, memory)), false)
    }

    #[must_use]
    pub fn start_in_nested_session(name: &str) -> Self {
        Self::start_at(Channel::Local, Some(name), true, None, false)
    }

    #[allow(clippy::too_many_lines)] // One fixture setup keeps its workspace, daemon, and Agent paths aligned.
    fn start_at(
        channel: Channel,
        session: Option<&str>,
        nested: bool,
        tool_availability: Option<(bool, bool)>,
        all_agents: bool,
    ) -> Self {
        let heavy_e2e = heavy_e2e_lock();
        let workspace = short_dir("usagi-mcp-workspace-");
        git(workspace.path(), &["init", "-q"]);
        git(
            workspace.path(),
            &["config", "user.email", "mcp-e2e@example.test"],
        );
        git(workspace.path(), &["config", "user.name", "MCP E2E"]);
        fs::write(workspace.path().join("README.md"), "fixture\n").unwrap();
        git(workspace.path(), &["add", "README.md"]);
        git(workspace.path(), &["commit", "-qm", "fixture"]);

        let home = short_dir("usagi-mcp-home-");
        // production channel では `$USAGI_HOME` 自体が daemon の data directory になるため、
        // 私有ディレクトリ（0700）でなければ endpoint 検証に落ちる。local channel の
        // `<home>/local` は usagi 自身が 0700 で作るのでこの差は現れない。
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700))
            .expect("private daemon data directory");
        configure_tool_availability(channel, home.path(), tool_availability);
        let fixture_bin = home.path().join("fixture-bin");
        let fixture_log = home.path().join("fixture-agent.log");
        let fixture_argv = home.path().join("fixture-argv");
        let fixture_mcp_input = home.path().join("fixture-mcp.in");
        let fixture_mcp_output = home.path().join("fixture-mcp.out");
        make_fifo(&fixture_mcp_input);
        make_fifo(&fixture_mcp_output);
        fs::create_dir(&fixture_bin).unwrap();
        fs::create_dir(&fixture_argv).unwrap();
        install_fixture_agent(
            &fixture_bin,
            "codex",
            &fixture_log,
            &fixture_argv,
            &fixture_mcp_input,
            &fixture_mcp_output,
        );
        install_fixture_agent(
            &fixture_bin,
            "claude",
            &fixture_log,
            &fixture_argv,
            &fixture_mcp_input,
            &fixture_mcp_output,
        );
        if all_agents {
            install_fixture_agent(
                &fixture_bin,
                "codex-fugu",
                &fixture_log,
                &fixture_argv,
                &fixture_mcp_input,
                &fixture_mcp_output,
            );
        }
        fs::create_dir(workspace.path().join(".usagi")).unwrap();
        fs::write(
            workspace.path().join(".usagi/config.toml"),
            if all_agents {
                "[agents.codex]\nmodels = [\"fixture-codex\"]\n[agents.claude]\nmodels = [\"fixture-claude\"]\n[agents.sakana-ai]\nmodels = [\"fixture-sakana\"]\n"
            } else {
                "[agents.codex]\nmodels = [\"fixture-codex\"]\n[agents.claude]\nmodels = [\"fixture-claude\"]\n"
            },
        )
        .unwrap();
        git(workspace.path(), &["add", ".usagi/config.toml"]);
        git(workspace.path(), &["commit", "-qm", "fixture agent config"]);
        let cwd = session.map_or_else(
            || workspace.path().to_path_buf(),
            |name| {
                let sessions = workspace.path().join(".usagi/sessions");
                fs::create_dir_all(&sessions).unwrap();
                let cwd = sessions.join(name);
                let branch = format!("test/{name}");
                git(
                    workspace.path(),
                    &[
                        "worktree",
                        "add",
                        "-q",
                        "-b",
                        &branch,
                        cwd.to_str().unwrap(),
                    ],
                );
                cwd
            },
        );
        let cwd = if nested {
            let nested = cwd.join("crates/core");
            fs::create_dir_all(&nested).unwrap();
            nested
        } else {
            cwd
        };

        let path = format!(
            "{}:{}",
            fixture_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        // A real managed session is reached only after its repository root has
        // been adopted. `daemon start` deliberately starts without selecting a
        // workspace, so follow it with the same explicit Selected handshake a
        // surface uses before asking the session-scoped MCP client to bind.
        // Cold-start admission intentionally refuses an otherwise unknown
        // `.usagi/sessions/*` path.
        if session.is_some() {
            let status = usagi_command(
                home.path(),
                channel,
                workspace.path(),
                &["daemon".as_ref(), "start".as_ref()],
            )
            .env("PATH", &path)
            .env(SANDBOX_PASSTHROUGH, "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("fixture daemon starts from its repository root");
            assert!(status.success(), "fixture root daemon did not start");
            let deadline = Instant::now() + Duration::from_secs(10);
            let stream = loop {
                if let Ok(stream) = connect_current(&channel.data_dir(home.path())) {
                    break stream;
                }
                assert!(
                    Instant::now() < deadline,
                    "fixture daemon socket was not published"
                );
                thread::sleep(Duration::from_millis(20));
            };
            let _opening = IpcClient::connect(
                stream,
                "mcp-fixture-workspace-opener".into(),
                OperationId::new().to_string(),
                ClientPolicy::cli(),
                shipping_build_identity(),
                ClientWorkspace::Selected {
                    root: usagi_core::infrastructure::paths::wire_workspace_root(
                        workspace.path().canonicalize().unwrap(),
                    ),
                },
            )
            .expect("fixture explicitly opens its repository root");
        }
        let mut child = usagi_command(home.path(), channel, &cwd, &["mcp".as_ref()])
            .env("PATH", &path)
            .env(SANDBOX_PASSTHROUGH, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("shipping usagi mcp process starts");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut harness = Self {
            workspace,
            cwd,
            home,
            channel,
            fixture_bin,
            fixture_log,
            fixture_argv,
            fixture_mcp_input,
            fixture_mcp_output,
            process: McpProcess {
                child: Some(child),
                stdin: Box::new(stdin),
                stdout: Box::new(stdout),
                next_id: 1,
            },
            _heavy_e2e: heavy_e2e,
        };
        let initialized = harness.request(
            "initialize",
            &json!({"protocolVersion":"2025-06-18","clientInfo":{"name":"production-e2e","version":"1"}}),
        );
        assert_eq!(initialized["result"]["serverInfo"]["name"], "usagi");
        initialized["result"]["capabilities"]["tools"]
            .as_object()
            .expect("initialize advertises tools");
        harness.initialized();
        harness
    }

    pub fn request(&mut self, method: &str, params: &Value) -> Value {
        let id = self.process.next_id;
        self.process.next_id += 1;
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        writeln!(self.process.stdin, "{request}").unwrap();
        self.process.stdin.flush().unwrap();
        let mut line = String::new();
        self.process.stdout.read_line(&mut line).unwrap();
        if line.is_empty() {
            let mut stderr = String::new();
            if let Some(child) = self.process.child.as_mut()
                && let Some(mut pipe) = child.stderr.take()
            {
                pipe.read_to_string(&mut stderr).unwrap();
            }
            panic!(
                "MCP process closed before response {id}: fixture={} stderr={stderr}",
                fs::read_to_string(&self.fixture_log).unwrap_or_default()
            );
        }
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], id);
        response
    }

    fn initialized(&mut self) {
        writeln!(
            self.process.stdin,
            "{}",
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .unwrap();
        self.process.stdin.flush().unwrap();
    }

    pub fn tool(&mut self, name: &str, arguments: &Value) -> Value {
        self.request("tools/call", &json!({"name": name, "arguments": arguments}))
    }

    pub fn tools(&mut self) -> Vec<Value> {
        self.request("tools/list", &json!({}))["result"]["tools"]
            .as_array()
            .unwrap()
            .clone()
    }

    #[must_use]
    pub fn workspace(&self) -> &Path {
        self.workspace.path()
    }

    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// この harness が起動した daemon の selected data directory。
    #[must_use]
    pub fn data_dir(&self) -> PathBuf {
        self.channel.data_dir(self.home.path())
    }

    /// `$USAGI_HOME`（mode を適用する前の base）。
    #[must_use]
    pub fn home(&self) -> &Path {
        self.home.path()
    }

    #[must_use]
    pub fn fixture_bin(&self) -> &Path {
        &self.fixture_bin
    }

    #[must_use]
    pub fn fixture_log(&self) -> &Path {
        &self.fixture_log
    }

    /// Captured child argv, preserving opaque argument boundaries with NUL
    /// framing. Callers deliberately decide what is safe to include in an
    /// assertion message.
    pub fn fixture_argv(&self) -> Vec<FixtureArgv> {
        let mut paths = fs::read_dir(&self.fixture_argv)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        paths.sort();
        paths
            .into_iter()
            .filter_map(|path| {
                let runtime = path
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .split('.')
                    .next()
                    .unwrap()
                    .to_owned();
                let bytes = fs::read(path).unwrap();
                if bytes.last() != Some(&0) {
                    return None;
                }
                let arguments = bytes
                    .split(|byte| *byte == 0)
                    .filter(|argument| !argument.is_empty())
                    .map(|argument| String::from_utf8(argument.to_vec()))
                    .collect::<Result<Vec<_>, _>>()
                    .ok()?;
                Some(FixtureArgv { runtime, arguments })
            })
            .collect()
    }

    pub fn enable_local_llm(&self) {
        let storage = Storage::new(self.data_dir());
        let mut settings = storage.load_settings().unwrap();
        settings.local_llm.enabled = true;
        storage.save_settings(&settings).unwrap();
    }

    /// Replace one fixture runtime before dispatching it. Follow-up MCP suites
    /// use this seam to make a worker call `agent_complete` or `agent_fail`
    /// without relying on a real provider login.
    pub fn replace_fixture_agent(&self, runtime: &str, script: &str) {
        assert!(matches!(runtime, "codex" | "claude"));
        let executable = self.fixture_bin.join(runtime);
        fs::write(
            &executable,
            materialize_fixture_script(script, &self.fixture_log, &self.fixture_argv),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Launch a long-lived caller Agent and switch this harness to the canonical
    /// MCP child which that Agent starts inside its own PTY process group.
    pub fn launch_caller(&mut self) -> String {
        self.launch_caller_with_channel(true)
    }

    /// Launchs the fixture Agent without requiring it to host an MCP facade.
    pub fn launch_caller_without_mcp(&mut self) {
        drop(self.launch_caller_with_channel(false));
    }

    fn launch_caller_with_channel(&mut self, connect_mcp: bool) -> String {
        let created = self.tool("session_create", &json!({"name":"mcp-caller"}));
        assert!(created.get("error").is_none(), "{created}");
        let mut client = self.daemon_client();
        let listed = client
            .request(DaemonRequest::Session {
                action: SessionAction::List,
                operation_id: OperationId::new().to_string(),
                payload: json!({}),
            })
            .unwrap();
        let body = match listed {
            DaemonReply::Accepted { body, .. } | DaemonReply::Ok(body) => body,
        };
        let workspace: WorkspaceId = serde_json::from_value(body["workspace_id"].clone()).unwrap();
        let session: SessionId = serde_json::from_value(
            body["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|session| session["name"] == "mcp-caller")
                .unwrap()["session_id"]
                .clone(),
        )
        .unwrap();
        let launched = client
            .request(DaemonRequest::Agent {
                operation_id: OperationId::new().to_string(),
                intent: AgentLaunchIntent {
                    workspace,
                    session: Some(session),
                    profile: Some(AgentProfileId::new("codex").unwrap()),
                },
            })
            .unwrap();
        assert!(matches!(launched, DaemonReply::Accepted { .. }));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if fs::read_to_string(&self.fixture_log)
                .is_ok_and(|log| log.lines().any(|line| line == "fixture-ready"))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "canonical MCP child was not started"
            );
            thread::sleep(Duration::from_millis(20));
        }
        if !connect_mcp {
            return String::new();
        }
        if let Some(mut child) = self.process.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let input = OpenOptions::new()
            .write(true)
            .open(&self.fixture_mcp_input)
            .unwrap();
        let output = OpenOptions::new()
            .read(true)
            .open(&self.fixture_mcp_output)
            .unwrap();
        self.process = McpProcess {
            child: None,
            stdin: Box::new(input),
            stdout: Box::new(BufReader::new(output)),
            next_id: 1,
        };
        let initialized = self.request(
            "initialize",
            &json!({"protocolVersion":"2025-06-18","clientInfo":{"name":"claimed-child-e2e","version":"1"}}),
        );
        assert_eq!(initialized["result"]["serverInfo"]["name"], "usagi");
        self.initialized();
        String::new()
    }

    /// Restart only the stdio MCP facade with one daemon-provisioned caller
    /// credential. The already-running shipping daemon remains authoritative.
    pub fn restart_with_credential(&mut self, credential: &str) {
        assert!(
            credential.is_empty(),
            "caller bearer must never leave the MCP child"
        );
        let previous_exits = fs::read_to_string(&self.fixture_log)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.starts_with("mcp-exit:"))
            .count();
        let placeholder = McpProcess {
            child: None,
            stdin: Box::new(OpenOptions::new().write(true).open("/dev/null").unwrap()),
            stdout: Box::new(BufReader::new(
                OpenOptions::new().read(true).open("/dev/null").unwrap(),
            )),
            next_id: 1,
        };
        drop(std::mem::replace(&mut self.process, placeholder));
        let deadline = Instant::now() + Duration::from_secs(1);
        let relay_restarted = loop {
            let exits = fs::read_to_string(&self.fixture_log)
                .unwrap_or_default()
                .lines()
                .filter(|line| line.starts_with("mcp-exit:"))
                .count();
            if exits > previous_exits {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(20));
        };
        if relay_restarted {
            let input = OpenOptions::new()
                .write(true)
                .open(&self.fixture_mcp_input)
                .unwrap();
            let output = OpenOptions::new()
                .read(true)
                .open(&self.fixture_mcp_output)
                .unwrap();
            self.process = McpProcess {
                child: None,
                stdin: Box::new(input),
                stdout: Box::new(BufReader::new(output)),
                next_id: 1,
            };
        } else {
            let path = format!(
                "{}:{}",
                self.fixture_bin.display(),
                std::env::var("PATH").unwrap_or_default()
            );
            let mut child = usagi_command(
                self.home.path(),
                self.channel,
                self.workspace.path(),
                &["mcp".as_ref()],
            )
            .env("PATH", path)
            .env(SANDBOX_PASSTHROUGH, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
            self.process = McpProcess {
                stdin: Box::new(child.stdin.take().unwrap()),
                stdout: Box::new(BufReader::new(child.stdout.take().unwrap())),
                child: Some(child),
                next_id: 1,
            };
        }
        let initialized = self.request(
            "initialize",
            &json!({"protocolVersion":"2025-06-18","clientInfo":{"name":"reconnected-child-e2e","version":"1"}}),
        );
        assert_eq!(initialized["result"]["serverInfo"]["name"], "usagi");
        self.initialized();
    }

    pub fn daemon_client(&self) -> IpcClient<std::os::unix::net::UnixStream> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(stream) = connect_current(&self.data_dir()) {
                return IpcClient::connect(
                    stream,
                    "mcp-production-e2e".into(),
                    OperationId::new().to_string(),
                    ClientPolicy::cli(),
                    shipping_build_identity(),
                    super::daemon::client_workspace(&self.data_dir()),
                )
                .unwrap();
            }
            assert!(Instant::now() < deadline, "daemon socket was not published");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

fn configure_tool_availability(channel: Channel, home: &Path, availability: Option<(bool, bool)>) {
    if let Some((issue_enabled, memory_enabled)) = availability {
        let data_dir = channel.data_dir(home);
        ensure_private_dir_all(&data_dir).unwrap();
        Storage::new(data_dir)
            .save_settings(&Settings {
                issue_enabled,
                memory_enabled,
                ..Settings::default()
            })
            .unwrap();
    }
}

impl Drop for McpHarness {
    fn drop(&mut self) {
        if let Some(child) = &mut self.process.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        // graceful stop がタイムアウトしても、record の exact incarnation まで落として
        // MCP 経由で autostart した daemon を残さない。
        reap(self.home.path());
    }
}

fn make_fifo(path: &Path) {
    let path = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: path is a NUL-terminated owned string and mode contains only permission bits.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
}

fn short_dir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in("/tmp")
        .expect("short paths keep Unix sockets below platform limits")
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

fn materialize_fixture_script(script: &str, log: &Path, argv: &Path) -> String {
    let script = script
        .replace("$USAGI_MCP_FIXTURE_LOG", log.to_str().unwrap())
        .replace("$USAGI_E2E_USAGI", env!("CARGO_BIN_EXE_usagi"));
    let capture = format!(
        "if ! [ \"$1\" = login ] || ! [ \"$2\" = status ]; then printf '%s\\0' \"$@\" > \"{}/${{0##*/}}.$$.argv\"; fi\n",
        argv.display()
    );
    script.strip_prefix("#!/bin/sh\n").map_or_else(
        || format!("{capture}{script}"),
        |body| format!("#!/bin/sh\n{capture}{body}"),
    )
}

fn install_fixture_agent(
    bin: &Path,
    name: &str,
    log: &Path,
    argv: &Path,
    input: &Path,
    output: &Path,
) {
    let relay_lock = input.with_extension("lock");
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = login ] && [ \"$2\" = status ]; then exit 0; fi\nprintf 'spawn:%s\\n' \"${{0##*/}}\" >> \"$USAGI_MCP_FIXTURE_LOG\"\nprintf 'credential:%s\\n' \"${{USAGI_MCP_CALLER_CREDENTIAL-unset}}\" >> \"$USAGI_MCP_FIXTURE_LOG\"\nprintf 'fixture-ready\\n' >> \"$USAGI_MCP_FIXTURE_LOG\"\nif mkdir \"{}\" 2>/dev/null; then\n  cd \"$USAGI_WORKSPACE_ROOT\" || exit 1\n  while true; do\n    \"$USAGI_E2E_USAGI\" mcp < \"{}\" > \"{}\" 2>&1\n    printf 'mcp-exit:%s\\n' \"$?\" >> \"$USAGI_MCP_FIXTURE_LOG\"\n  done\nelse\n  while IFS= read -r line; do printf 'fixture-input:%s\\n' \"$line\"; done\nfi\n",
        relay_lock.display(),
        input.display(),
        output.display()
    );
    let executable = bin.join(name);
    fs::write(&executable, materialize_fixture_script(&script, log, argv)).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
}
