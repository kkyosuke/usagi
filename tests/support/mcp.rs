//! Production-process MCP test support.
//!
//! Tests using this module talk to the shipping `usagi mcp` binary over its
//! stdio JSON-RPC interface. The MCP process autostarts the shipping daemon;
//! both global data and the git workspace are isolated per harness.

#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use usagi_core::domain::agent::AgentProfileId;
use usagi_core::domain::id::{OperationId, SessionId, WorkspaceId};
use usagi_core::usecase::client::{
    AgentLaunchIntent, ClientPolicy, DaemonClient, DaemonReply, DaemonRequest, IpcClient,
    SessionAction,
};
use usagi_daemon::infrastructure::unix_transport::connect_current;

pub struct McpHarness {
    workspace: tempfile::TempDir,
    cwd: PathBuf,
    home: tempfile::TempDir,
    fixture_bin: PathBuf,
    fixture_log: PathBuf,
    process: McpProcess,
}

struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpHarness {
    #[must_use]
    pub fn start() -> Self {
        Self::start_at(None)
    }

    #[must_use]
    pub fn start_in_session(name: &str) -> Self {
        Self::start_at(Some(name))
    }

    fn start_at(session: Option<&str>) -> Self {
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
        let fixture_bin = home.path().join("fixture-bin");
        let fixture_log = home.path().join("fixture-agent.log");
        fs::create_dir(&fixture_bin).unwrap();
        install_fixture_agent(&fixture_bin, "codex");
        install_fixture_agent(&fixture_bin, "claude");
        fs::create_dir(workspace.path().join(".usagi")).unwrap();
        fs::write(
            workspace.path().join(".usagi/config.toml"),
            "[agents.codex]\nmodels = [\"fixture-codex\"]\n[agents.claude]\nmodels = [\"fixture-claude\"]\n",
        )
        .unwrap();
        git(workspace.path(), &["add", ".usagi/config.toml"]);
        git(workspace.path(), &["commit", "-qm", "fixture agent config"]);
        let cwd = session.map_or_else(
            || workspace.path().to_path_buf(),
            |name| workspace.path().join(".usagi/sessions").join(name),
        );
        fs::create_dir_all(&cwd).unwrap();

        let path = format!(
            "{}:{}",
            fixture_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut child = Command::new(env!("CARGO_BIN_EXE_usagi"))
            .arg("mcp")
            .current_dir(&cwd)
            .env("USAGI_HOME", home.path())
            .env("USAGI_MCP_FIXTURE_LOG", &fixture_log)
            .env("USAGI_E2E_USAGI", env!("CARGO_BIN_EXE_usagi"))
            .env("PATH", path)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_INDEX_FILE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("shipping usagi mcp process starts");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut harness = Self {
            workspace,
            cwd,
            home,
            fixture_bin,
            fixture_log,
            process: McpProcess {
                child,
                stdin,
                stdout,
                next_id: 1,
            },
        };
        let initialized = harness.request(
            "initialize",
            &json!({"protocolVersion":"2025-06-18","clientInfo":{"name":"production-e2e","version":"1"}}),
        );
        assert_eq!(initialized["result"]["serverInfo"]["name"], "usagi");
        initialized["result"]["capabilities"]["tools"]
            .as_object()
            .expect("initialize advertises tools");
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
        assert!(!line.is_empty(), "MCP process closed before response {id}");
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], id);
        response
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

    #[must_use]
    pub fn data_dir(&self) -> PathBuf {
        usagi_core::infrastructure::paths::channel_data_dir(self.home.path())
    }

    #[must_use]
    pub fn fixture_bin(&self) -> &Path {
        &self.fixture_bin
    }

    #[must_use]
    pub fn fixture_log(&self) -> &Path {
        &self.fixture_log
    }

    /// Replace one fixture runtime before dispatching it. Follow-up MCP suites
    /// use this seam to make a worker call `agent_complete` or `agent_fail`
    /// without relying on a real provider login.
    pub fn replace_fixture_agent(&self, runtime: &str, script: &str) {
        assert!(matches!(runtime, "codex" | "claude"));
        let executable = self.fixture_bin.join(runtime);
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Launch a long-lived caller Agent through the shipping daemon and return
    /// the opaque MCP credential injected into that exact runtime.
    pub fn launch_caller(&mut self) -> String {
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
            if let Ok(log) = fs::read_to_string(&self.fixture_log)
                && let Some(credential) = log
                    .lines()
                    .find_map(|line| line.strip_prefix("credential:"))
            {
                return credential.to_owned();
            }
            assert!(
                Instant::now() < deadline,
                "caller credential was not provisioned"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Restart only the stdio MCP facade with one daemon-provisioned caller
    /// credential. The already-running shipping daemon remains authoritative.
    pub fn restart_with_credential(&mut self, credential: &str) {
        let _ = self.process.child.kill();
        let _ = self.process.child.wait();
        let path = format!(
            "{}:{}",
            self.fixture_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut child = Command::new(env!("CARGO_BIN_EXE_usagi"))
            .arg("mcp")
            .current_dir(self.workspace.path())
            .env("USAGI_HOME", self.home.path())
            .env("USAGI_MCP_FIXTURE_LOG", &self.fixture_log)
            .env("USAGI_E2E_USAGI", env!("CARGO_BIN_EXE_usagi"))
            .env("USAGI_MCP_CALLER_CREDENTIAL", credential)
            .env("PATH", path)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_INDEX_FILE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.process = McpProcess {
            stdin: child.stdin.take().unwrap(),
            stdout: BufReader::new(child.stdout.take().unwrap()),
            child,
            next_id: 1,
        };
        let initialized = self.request(
            "initialize",
            &json!({"protocolVersion":"2025-06-18","clientInfo":{"name":"credential-e2e","version":"1"}}),
        );
        assert_eq!(initialized["result"]["serverInfo"]["name"], "usagi");
    }

    fn daemon_client(&self) -> IpcClient<std::os::unix::net::UnixStream> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(stream) = connect_current(&self.data_dir()) {
                return IpcClient::connect(
                    stream,
                    "mcp-production-e2e".into(),
                    OperationId::new().to_string(),
                    ClientPolicy::cli(),
                )
                .unwrap();
            }
            assert!(Instant::now() < deadline, "daemon socket was not published");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for McpHarness {
    fn drop(&mut self) {
        let _ = self.process.child.kill();
        let _ = self.process.child.wait();
        let _ = Command::new(env!("CARGO_BIN_EXE_usagi"))
            .args(["daemon", "stop"])
            .current_dir(self.workspace.path())
            .env("USAGI_HOME", self.home.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
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

fn install_fixture_agent(bin: &Path, name: &str) {
    let script = "#!/bin/sh\nif [ \"$1\" = login ] && [ \"$2\" = status ]; then exit 0; fi\nprintf '%s\\n' \"$0 $*\" >> \"$USAGI_MCP_FIXTURE_LOG\"\nprintf 'credential:%s\\n' \"$USAGI_MCP_CALLER_CREDENTIAL\" >> \"$USAGI_MCP_FIXTURE_LOG\"\nprintf 'fixture-ready\\n'\nwhile IFS= read -r line; do printf 'fixture-input:%s\\n' \"$line\"; done\n";
    let executable = bin.join(name);
    fs::write(&executable, script).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
}
