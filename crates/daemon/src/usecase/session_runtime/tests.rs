//! session runtime の振る舞いを固定するテスト。

use super::*;
use crate::infrastructure::session_worktree::SystemSessionWorktreeIo;
use crate::usecase::session_teardown::drain_pending_teardowns;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;
use usagi_core::domain::session_lifecycle::{ManagedSession, SessionLifecycle};
use usagi_core::infrastructure::git::GitOutput;

struct FakeSessionGit(bool);
impl FakeSessionGit {
    fn ok() -> Self {
        Self(true)
    }
    fn fail() -> Self {
        Self(false)
    }
}

struct FakeSessionWorktreeIo {
    occupied: bool,
    build_calls: Arc<AtomicUsize>,
}

struct SetupSessionWorktreeIo {
    calls: Arc<Mutex<Vec<(PathBuf, String)>>>,
    fail_on: Option<String>,
    runtime: Arc<Mutex<Option<std::sync::Weak<Mutex<SessionRuntime>>>>>,
    observed_unlocked: Arc<std::sync::atomic::AtomicBool>,
}

fn pending_create(step: SessionCreateStep) -> Option<Box<SessionCreateInFlight>> {
    match step {
        SessionCreateStep::Pending(in_flight) => Some(in_flight),
        SessionCreateStep::Done(_) => None,
    }
}

fn pending_initialize(completion: SessionCreateCompletion) -> Option<SessionInitializeInFlight> {
    match completion {
        SessionCreateCompletion::Initializing(in_flight) => Some(in_flight),
        SessionCreateCompletion::Done(_) => None,
    }
}

struct OrphanSessionWorktreeIo {
    entries: Vec<String>,
    linked: bool,
    remove_calls: Arc<AtomicUsize>,
}

impl SessionWorktreeIo for OrphanSessionWorktreeIo {
    fn remove_file_best_effort(&self, _: &Path) {}
    fn path_occupied(&self, _: &Path) -> bool {
        true
    }
    fn canonical_path(&self, path: &Path) -> Option<PathBuf> {
        Some(path.into())
    }
    fn is_repo_root(&self, _: &Path) -> bool {
        false
    }
    fn is_linked_worktree(&self, _: &Path) -> bool {
        self.linked
    }
    fn session_entries(&self, _: &Path) -> anyhow::Result<Vec<String>> {
        Ok(self.entries.clone())
    }
    fn build_session_tree(
        &self,
        _: &dyn GitRunner,
        _: &Path,
        _: &Path,
        _: &str,
        _: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn run_setup_command(&self, _: &Path, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn remove_session_tree(&self, _: &dyn GitRunner, _: &Path, _: bool) -> anyhow::Result<()> {
        self.remove_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct OrphanGit {
    branch: &'static str,
    dirty: bool,
    unmerged_commits: u64,
    calls: Arc<Mutex<Vec<Vec<String>>>>,
}

impl GitRunner for OrphanGit {
    fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|arg| (*arg).to_owned()).collect());
        let stdout = match args {
            ["symbolic-ref", "--quiet", "--short", "HEAD"] => {
                format!("{}\n", self.branch)
            }
            ["status", "--porcelain"] if self.dirty => " M protected.rs\n".into(),
            ["rev-parse", "HEAD"] => "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n".into(),
            ["rev-list", "--count", _] => format!("{}\n", self.unmerged_commits),
            _ => String::new(),
        };
        Ok(GitOutput {
            success: true,
            stdout,
            stderr: String::new(),
        })
    }
}

struct FailingSessionWorktreeIo;

struct ConfinementIo {
    canonical: std::collections::BTreeMap<PathBuf, Option<PathBuf>>,
    occupied: bool,
    remove_calls: Arc<AtomicUsize>,
}

impl ConfinementIo {
    fn new(remove_calls: Arc<AtomicUsize>) -> Self {
        Self {
            canonical: std::collections::BTreeMap::new(),
            occupied: false,
            remove_calls,
        }
    }
}

impl SessionWorktreeIo for ConfinementIo {
    fn remove_file_best_effort(&self, _: &Path) {}
    fn path_occupied(&self, _: &Path) -> bool {
        self.occupied
    }
    fn canonical_path(&self, path: &Path) -> Option<PathBuf> {
        self.canonical
            .get(path)
            .cloned()
            .unwrap_or_else(|| Some(path.into()))
    }
    fn is_repo_root(&self, _: &Path) -> bool {
        false
    }
    fn is_linked_worktree(&self, _: &Path) -> bool {
        false
    }
    fn build_session_tree(
        &self,
        _: &dyn GitRunner,
        _: &Path,
        _: &Path,
        _: &str,
        _: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn run_setup_command(&self, _: &Path, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn remove_session_tree(&self, _: &dyn GitRunner, _: &Path, _: bool) -> anyhow::Result<()> {
        self.remove_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl SessionWorktreeIo for FailingSessionWorktreeIo {
    fn remove_file_best_effort(&self, _: &Path) {}
    fn path_occupied(&self, _: &Path) -> bool {
        false
    }
    fn canonical_path(&self, path: &Path) -> Option<PathBuf> {
        Some(path.into())
    }
    fn is_repo_root(&self, _: &Path) -> bool {
        false
    }
    fn is_linked_worktree(&self, _: &Path) -> bool {
        false
    }
    fn build_session_tree(
        &self,
        _: &dyn GitRunner,
        _: &Path,
        _: &Path,
        _: &str,
        _: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn run_setup_command(&self, _: &Path, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn remove_session_tree(&self, _: &dyn GitRunner, _: &Path, _: bool) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("injected remove failure"))
    }
}

impl SessionWorktreeIo for FakeSessionWorktreeIo {
    fn remove_file_best_effort(&self, _: &Path) {}

    fn path_occupied(&self, _: &Path) -> bool {
        self.occupied
    }

    fn canonical_path(&self, path: &Path) -> Option<PathBuf> {
        Some(path.to_path_buf())
    }

    fn is_repo_root(&self, _: &Path) -> bool {
        false
    }

    fn is_linked_worktree(&self, _: &Path) -> bool {
        true
    }

    fn build_session_tree(
        &self,
        git: &dyn GitRunner,
        workspace_root: &Path,
        _: &Path,
        _: &str,
        _: Option<&str>,
    ) -> anyhow::Result<()> {
        self.build_calls.fetch_add(1, Ordering::SeqCst);
        let output = git.run(workspace_root, &["worktree", "add"])?;
        if output.success {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "git worktree add failed: {}",
                output.stderr
            ))
        }
    }

    fn run_setup_command(&self, _: &Path, _: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn remove_session_tree(&self, _: &dyn GitRunner, _: &Path, _: bool) -> anyhow::Result<()> {
        Ok(())
    }
}

impl SessionWorktreeIo for SetupSessionWorktreeIo {
    fn remove_file_best_effort(&self, _: &Path) {}

    fn path_occupied(&self, _: &Path) -> bool {
        false
    }

    fn canonical_path(&self, path: &Path) -> Option<PathBuf> {
        Some(path.to_path_buf())
    }

    fn is_repo_root(&self, _: &Path) -> bool {
        false
    }

    fn is_linked_worktree(&self, _: &Path) -> bool {
        true
    }

    fn build_session_tree(
        &self,
        _: &dyn GitRunner,
        _: &Path,
        _: &Path,
        _: &str,
        _: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn run_setup_command(&self, session_root: &Path, command: &str) -> anyhow::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push((session_root.to_path_buf(), command.to_owned()));
        if let Some(runtime) = self
            .runtime
            .lock()
            .unwrap()
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            && runtime.try_lock().is_ok()
        {
            self.observed_unlocked.store(true, Ordering::SeqCst);
        }
        if self.fail_on.as_deref() == Some(command) {
            Err(anyhow::anyhow!("injected setup failure"))
        } else {
            Ok(())
        }
    }

    fn remove_session_tree(&self, _: &dyn GitRunner, _: &Path, _: bool) -> anyhow::Result<()> {
        Ok(())
    }
}

#[test]
fn specialized_worktree_fakes_keep_their_noop_contracts_explicit() {
    let session_root = Path::new("session");
    let remove_calls = Arc::new(AtomicUsize::new(0));
    let orphan = OrphanSessionWorktreeIo {
        entries: Vec::new(),
        linked: false,
        remove_calls: Arc::clone(&remove_calls),
    };
    orphan.run_setup_command(session_root, "ignored").unwrap();

    let confinement = ConfinementIo::new(Arc::clone(&remove_calls));
    confinement
        .run_setup_command(session_root, "ignored")
        .unwrap();
    FailingSessionWorktreeIo
        .run_setup_command(session_root, "ignored")
        .unwrap();

    let fake = FakeSessionWorktreeIo {
        occupied: false,
        build_calls: Arc::new(AtomicUsize::new(0)),
    };
    fake.run_setup_command(session_root, "ignored").unwrap();

    let setup = SetupSessionWorktreeIo {
        calls: Arc::new(Mutex::new(Vec::new())),
        fail_on: None,
        runtime: Arc::new(Mutex::new(None)),
        observed_unlocked: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    assert_eq!(
        setup.canonical_path(session_root),
        Some(session_root.into())
    );
    assert!(setup.is_linked_worktree(session_root));
    setup
        .remove_session_tree(&FakeSessionGit::ok(), session_root, false)
        .unwrap();
}

#[test]
fn create_step_extractors_distinguish_completed_steps() {
    let reply = SessionReply {
        operation_id: String::new(),
        revision: 0,
        body: Value::Null,
    };
    assert!(pending_create(SessionCreateStep::Done(reply.clone())).is_none());
    assert!(pending_initialize(SessionCreateCompletion::Done(reply)).is_none());
}

struct BranchExistsGit;
fn checkout_validation_output(args: &[&str]) -> Option<GitOutput> {
    if matches!(
        args,
        ["rev-parse", "--verify", "--end-of-options", expression]
            if expression.ends_with("^{commit}")
    ) {
        return Some(GitOutput {
            success: true,
            stdout: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            stderr: String::new(),
        });
    }
    (args.first() == Some(&"ls-tree")).then(|| GitOutput {
        success: true,
        stdout: String::new(),
        stderr: String::new(),
    })
}

impl GitRunner for BranchExistsGit {
    fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
        if let Some(output) = checkout_validation_output(args) {
            return Ok(output);
        }
        Ok(GitOutput {
            success: false,
            stdout: String::new(),
            stderr: "fatal: a branch named 'usagi/one' already exists".into(),
        })
    }
}

struct WorkspaceExistsGit;
impl GitRunner for WorkspaceExistsGit {
    fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
        if let Some(output) = checkout_validation_output(args) {
            return Ok(output);
        }
        if matches!(args, ["branch", "--", ..] | ["branch", "-D", "--", ..])
            || matches!(args, ["worktree", "list", "--porcelain"])
        {
            return Ok(GitOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            });
        }
        Ok(GitOutput {
            success: false,
            stdout: String::new(),
            stderr: "fatal: '/repo/.usagi/sessions/one' already exists".into(),
        })
    }
}
impl GitRunner for FakeSessionGit {
    fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
        if let Some(output) = checkout_validation_output(args) {
            return Ok(output);
        }
        Ok(GitOutput {
            success: self.0,
            stdout: String::new(),
            stderr: "no".into(),
        })
    }
}

enum ScriptedGitResult {
    Output {
        success: bool,
        stdout: &'static str,
        stderr: &'static str,
    },
    Error,
}

struct ScriptedGit {
    results: Mutex<std::collections::VecDeque<ScriptedGitResult>>,
}

impl ScriptedGit {
    fn new(results: impl IntoIterator<Item = ScriptedGitResult>) -> Self {
        Self {
            results: Mutex::new(results.into_iter().collect()),
        }
    }
}

impl GitRunner for ScriptedGit {
    fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
        if let Some(output) = checkout_validation_output(args) {
            return Ok(output);
        }
        match self.results.lock().unwrap().pop_front().unwrap() {
            ScriptedGitResult::Output {
                success,
                stdout,
                stderr,
            } => Ok(GitOutput {
                success,
                stdout: stdout.into(),
                stderr: stderr.into(),
            }),
            ScriptedGitResult::Error => Err(anyhow::anyhow!("injected Git IO failure")),
        }
    }
}

struct CountingGit {
    calls: Arc<AtomicUsize>,
}

struct OutcomeGit {
    succeeds: bool,
    calls: Arc<AtomicUsize>,
}

type GitCall = (PathBuf, Vec<String>);
type RecordingCalls = Arc<Mutex<Vec<GitCall>>>;

struct RecordingGit {
    calls: RecordingCalls,
}
impl RecordingGit {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}
impl GitRunner for RecordingGit {
    fn run(&self, repo: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
        self.calls.lock().unwrap().push((
            repo.into(),
            args.iter().map(|arg| (*arg).to_owned()).collect(),
        ));
        Ok(checkout_validation_output(args).unwrap_or(GitOutput {
            success: true,
            stdout: String::new(),
            stderr: String::new(),
        }))
    }
}
impl GitRunner for CountingGit {
    fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(output) = checkout_validation_output(args) {
            return Ok(output);
        }
        Ok(GitOutput {
            success: true,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}
impl GitRunner for OutcomeGit {
    fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(output) = checkout_validation_output(args) {
            return Ok(output);
        }
        Ok(GitOutput {
            success: self.succeeds,
            stdout: String::new(),
            stderr: "injected effect failure".into(),
        })
    }
}
/// A teardown that always refuses, standing in for a worktree Git will not
/// remove (dirty, busy, or permission-denied).
struct FailingTeardown;
impl TeardownEffect for FailingTeardown {
    fn tear_down(&self, _: &PendingTeardown) -> Result<(), String> {
        Err("fatal: 'one' contains modified or untracked files".into())
    }
}

fn runtime(git: FakeSessionGit) -> (TempDir, SessionRuntime) {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    let runtime = SessionRuntime::open(
        tmp.path().to_path_buf(),
        &tmp.path().join("daemon"),
        DaemonGeneration::new(),
        git,
        SystemSessionWorktreeIo,
    )
    .unwrap();
    (tmp, runtime)
}
fn operation() -> String {
    OperationId::new().to_string()
}

fn confined_teardown() -> PendingTeardown {
    PendingTeardown {
        session_id: SessionId::new(),
        operation_id: OperationId::new(),
        name: "one".into(),
        repository_root: PathBuf::from("/repo"),
        data_home: PathBuf::from("/data"),
        session_container: PathBuf::from("/repo/.usagi/sessions"),
        session_root: PathBuf::from("/repo/.usagi/sessions/one"),
        force: false,
        delete_branch: false,
        branch_name: None,
        force_delete_branch: false,
        merged_head_oid: None,
    }
}

#[test]
fn a_branch_preserving_teardown_skips_git_branch_deletion() {
    assert_eq!(
        delete_teardown_branch(&FakeSessionGit::ok(), &confined_teardown()),
        Ok(())
    );
}

#[test]
fn a_stored_conventional_branch_is_deleted_only_once() {
    let git = RecordingGit::new();
    let calls = Arc::clone(&git.calls);
    let mut teardown = confined_teardown();
    teardown.delete_branch = true;
    teardown.branch_name = Some("usagi/one".into());

    delete_teardown_branch(&git, &teardown).unwrap();

    assert_eq!(
        calls.lock().unwrap().as_slice(),
        &[(
            PathBuf::from("/repo"),
            vec![
                "branch".into(),
                "-d".into(),
                "--".into(),
                "usagi/one".into()
            ],
        )]
    );
}

#[test]
fn an_exact_merged_pr_head_force_deletes_only_that_squash_merged_branch() {
    struct HeadRecordingGit {
        head: String,
        calls: Arc<Mutex<Vec<Vec<String>>>>,
    }
    impl GitRunner for HeadRecordingGit {
        fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|arg| (*arg).to_owned()).collect());
            Ok(GitOutput {
                success: true,
                stdout: if args.first() == Some(&"rev-parse") {
                    self.head.clone()
                } else {
                    String::new()
                },
                stderr: String::new(),
            })
        }
    }

    for (head, expected_flag) in [("a".repeat(40), "-D"), ("b".repeat(40), "-d")] {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut teardown = confined_teardown();
        teardown.delete_branch = true;
        teardown.merged_head_oid = Some("a".repeat(40));
        delete_teardown_branch(
            &HeadRecordingGit {
                head,
                calls: Arc::clone(&calls),
            },
            &teardown,
        )
        .unwrap();
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .any(|args| { args == &["branch", expected_flag, "--", "usagi/one"] })
        );
        assert_eq!(
            calls.lock().unwrap()[0],
            ["rev-parse", "--verify", "refs/heads/usagi/one"]
        );
    }
}

#[test]
fn merged_pr_head_is_durable_across_teardown_worker_handoff() {
    let (tmp, rt) = runtime(FakeSessionGit::ok());
    let runtime = Arc::new(Mutex::new(rt));
    perform_create(
        &runtime,
        &FakeSessionGit::ok(),
        &operation(),
        &json!({"name":"one"}),
    )
    .unwrap();
    let signal = TeardownSignal::new();
    let head = "a".repeat(40);
    perform_remove_with_merged_head(
        &runtime,
        &signal,
        &operation(),
        &json!({"name":"one"}),
        Some(head.clone()),
    )
    .unwrap();

    let pending = runtime.lock().unwrap().pending_teardowns().unwrap();
    assert_eq!(pending[0].merged_head_oid.as_deref(), Some(head.as_str()));
    let reopened = SessionRuntime::open(
        tmp.path().to_path_buf(),
        &tmp.path().join("daemon"),
        DaemonGeneration::new(),
        FakeSessionGit::ok(),
        SystemSessionWorktreeIo,
    )
    .unwrap();
    let state = reopened.state().unwrap();
    assert_eq!(
        state.sessions[0]
            .delete_plan
            .as_ref()
            .unwrap()
            .merged_head_oid
            .as_deref(),
        Some(head.as_str())
    );
}

#[test]
fn removal_identity_treats_a_missing_branch_as_absent_and_rejects_unknown_names() {
    let (_tmp, rt) = runtime(FakeSessionGit::fail());
    let runtime = Arc::new(Mutex::new(rt));
    perform_create(
        &runtime,
        &FakeSessionGit::ok(),
        &operation(),
        &json!({"name":"one"}),
    )
    .unwrap();
    let runtime = runtime.lock().unwrap();
    let session_id = runtime.session_id("one").unwrap();
    assert_eq!(runtime.removal_identity("one").unwrap(), (session_id, None));
    assert_eq!(
        runtime.removal_identity("missing"),
        Err(SessionRuntimeError::UnknownSession)
    );
}

#[test]
fn session_runtime_fake_git_contract() {
    let tmp = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut runtime = SessionRuntime::open(
        tmp.path().join("repository"),
        &tmp.path().join("daemon"),
        DaemonGeneration::new(),
        FakeSessionGit::fail(),
        FakeSessionWorktreeIo {
            occupied: false,
            build_calls: Arc::clone(&calls),
        },
    )
    .unwrap();

    let error = runtime
        .handle(SessionAction::Create, &operation(), &json!({"name": "one"}))
        .unwrap_err();

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        error,
        SessionRuntimeError::SessionWorkspaceCreationFailed {
            name: "one".into(),
            detail: "no".into(),
        }
    );
}

#[test]
fn session_runtime_fake_fs_contract() {
    let tmp = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut runtime = SessionRuntime::open(
        tmp.path().join("repository"),
        &tmp.path().join("daemon"),
        DaemonGeneration::new(),
        FakeSessionGit::ok(),
        FakeSessionWorktreeIo {
            occupied: true,
            build_calls: Arc::clone(&calls),
        },
    )
    .unwrap();

    let error = runtime
        .handle(
            SessionAction::Create,
            &operation(),
            &json!({"name": "occupied"}),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        SessionRuntimeError::OrphanRecoveryBlocked(_)
    ));
    let row = &runtime.snapshot().unwrap()["sessions"][0];
    assert_eq!(row["name"], "occupied");
    assert_eq!(row["lifecycle"], "failed");
    assert_eq!(row["failure"]["stage"], "integrity");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn startup_adopts_a_clean_merged_orphan_and_safe_remove_deletes_actual_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let remove_calls = Arc::new(AtomicUsize::new(0));
    let git_calls = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = SessionRuntime::open(
        tmp.path().join("repository"),
        &tmp.path().join("daemon"),
        DaemonGeneration::new(),
        OrphanGit {
            branch: "usagi/retained-work",
            dirty: false,
            unmerged_commits: 0,
            calls: Arc::clone(&git_calls),
        },
        OrphanSessionWorktreeIo {
            entries: vec!["review".into()],
            linked: true,
            remove_calls: Arc::clone(&remove_calls),
        },
    )
    .unwrap();

    let coverage_path = Path::new("/coverage/orphan");
    assert_eq!(
        runtime.io.canonical_path(coverage_path),
        Some(coverage_path.into())
    );
    runtime
        .io
        .build_session_tree(
            runtime.git.as_ref(),
            coverage_path,
            coverage_path,
            "usagi/coverage",
            None,
        )
        .unwrap();

    let row = &runtime.snapshot().unwrap()["sessions"][0];
    assert_eq!(row["name"], "review");
    assert_eq!(row["lifecycle"], "failed");
    assert_eq!(row["failure"]["stage"], "integrity");
    assert_eq!(
        row["failure"]["summary"],
        "orphan session \"review\": branch=usagi/retained-work, dirty=false, unmerged_commits=0; safe cleanup is available"
    );
    assert_eq!(
        runtime.session_id("review"),
        Err(SessionRuntimeError::UnknownSession),
        "adoption must not make the orphan attachable"
    );

    let removed = runtime
        .handle(
            SessionAction::Remove,
            &operation(),
            &json!({"name":"review"}),
        )
        .unwrap();
    assert!(removed.body["sessions"].as_array().unwrap().is_empty());
    assert_eq!(remove_calls.load(Ordering::SeqCst), 1);
    let calls = git_calls.lock().unwrap();
    assert!(
        calls
            .iter()
            .any(|args| { args == &["branch", "-d", "--", "usagi/retained-work"] })
    );
    assert!(
        calls
            .iter()
            .any(|args| args == &["branch", "-d", "--", "usagi/review"])
    );
}

#[test]
fn a_manually_removed_orphan_is_safe_to_forget() {
    let (_tmp, runtime) = runtime(FakeSessionGit::ok());

    let diagnosis = runtime.inspect_orphan("gone");

    assert!(diagnosis.safe_to_remove());
    assert_eq!(
        diagnosis.summary("gone"),
        "orphan session \"gone\" no longer has a worktree; cleanup can remove its stale lifecycle row"
    );
}

#[test]
fn a_detached_or_outside_namespace_orphan_requires_manual_recovery() {
    let diagnosis = OrphanDiagnosis {
        branch: None,
        dirty: Some(false),
        unmerged_commits: Some(0),
        path_present: true,
        linked_worktree: true,
    };

    assert!(!diagnosis.safe_to_remove());
    assert_eq!(
        diagnosis.summary("detached"),
        "orphan session \"detached\": branch=unknown, dirty=false, unmerged_commits=0; cleanup blocked: the checked-out branch is detached or outside the usagi/ namespace"
    );
}

#[test]
fn dirty_or_unmerged_orphans_refuse_ordinary_force_with_guidance() {
    for (dirty, unmerged_commits, expected) in [
        (true, 0, "commit or stash local changes first"),
        (false, 3, "preserve the branch and open/merge a PR first"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let remove_calls = Arc::new(AtomicUsize::new(0));
        let mut runtime = SessionRuntime::open(
            tmp.path().join("repository"),
            &tmp.path().join("daemon"),
            DaemonGeneration::new(),
            OrphanGit {
                branch: "usagi/protected-work",
                dirty,
                unmerged_commits,
                calls: Arc::new(Mutex::new(Vec::new())),
            },
            OrphanSessionWorktreeIo {
                entries: vec!["review".into()],
                linked: true,
                remove_calls: Arc::clone(&remove_calls),
            },
        )
        .unwrap();

        let error = runtime
            .handle(
                SessionAction::Remove,
                &operation(),
                &json!({
                    "name":"review",
                    "force":true
                }),
            )
            .unwrap_err();
        let message = error.safe_message();
        assert!(message.contains(expected), "{message}");
        assert!(message.contains(&format!("unmerged_commits={unmerged_commits}")));
        assert_eq!(remove_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            runtime.snapshot().unwrap()["sessions"][0]["lifecycle"],
            "failed"
        );
    }
}

#[test]
fn explicit_orphan_purge_removes_unregistered_files_and_unmerged_work() {
    for linked in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let remove_calls = Arc::new(AtomicUsize::new(0));
        let git_calls = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = SessionRuntime::open(
            tmp.path().join("repository"),
            &tmp.path().join("daemon"),
            DaemonGeneration::new(),
            OrphanGit {
                branch: "usagi/protected-work",
                dirty: true,
                unmerged_commits: 3,
                calls: Arc::clone(&git_calls),
            },
            OrphanSessionWorktreeIo {
                entries: vec!["review".into()],
                linked,
                remove_calls: Arc::clone(&remove_calls),
            },
        )
        .unwrap();

        let removed = runtime
            .handle(
                SessionAction::Remove,
                &operation(),
                &json!({
                    "name":"review",
                    "force":true,
                    "purge_orphan":true
                }),
            )
            .unwrap();

        assert!(removed.body["sessions"].as_array().unwrap().is_empty());
        assert_eq!(remove_calls.load(Ordering::SeqCst), 1);
        let calls = git_calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|args| { args == &["branch", "-D", "--", "usagi/review"] })
        );
        if linked {
            assert!(
                calls
                    .iter()
                    .any(|args| { args == &["branch", "-D", "--", "usagi/protected-work"] })
            );
        }
    }
}

#[test]
fn explicit_orphan_purge_rejects_a_registered_session() {
    let (_tmp, mut runtime) = runtime(FakeSessionGit::ok());
    runtime
        .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
        .unwrap();

    let error = runtime
        .handle(
            SessionAction::Remove,
            &operation(),
            &json!({
                "name":"one",
                "force":true,
                "purge_orphan":true
            }),
        )
        .unwrap_err();

    assert_eq!(error, SessionRuntimeError::InvalidRequest);
    assert_eq!(runtime.snapshot().unwrap()["sessions"][0]["name"], "one");
}

#[test]
fn create_lists_overview_and_removes_a_durable_session() {
    let (_tmp, mut runtime) = runtime(FakeSessionGit::ok());
    // An empty workspace has nothing only its owner can finish, so it may be
    // given back; a session mid-teardown is exactly such work.
    assert!(!runtime.has_unfinished_work().unwrap());
    let created = runtime
        .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
        .unwrap();
    assert_eq!(created.body["sessions"].as_array().unwrap().len(), 1);
    assert!(!runtime.has_unfinished_work().unwrap());
    let list = runtime
        .handle(SessionAction::List, "read", &json!({}))
        .unwrap();
    assert_eq!(list.revision, created.revision);
    let overview = runtime
        .handle(SessionAction::Overview, "read", &json!({}))
        .unwrap();
    assert_eq!(overview.body, list.body);
    let removed = runtime
        .handle(SessionAction::Remove, &operation(), &json!({"name":"one"}))
        .unwrap();
    assert!(removed.body["sessions"].as_array().unwrap().is_empty());
}

#[test]
fn creates_a_single_character_session_name() {
    let (_tmp, mut runtime) = runtime(FakeSessionGit::ok());

    let created = runtime
        .handle(SessionAction::Create, &operation(), &json!({"name":"a"}))
        .unwrap();

    assert_eq!(created.body["sessions"][0]["name"], "a");
    assert_eq!(created.body["sessions"][0]["lifecycle"], "available");
}

#[test]
fn session_base_ref_accepts_only_fully_qualified_branch_refs() {
    assert_eq!(session_base_ref(&json!({})).unwrap(), None);
    assert_eq!(
        session_base_ref(&json!({"base_ref":"refs/heads/main"})).unwrap(),
        Some("refs/heads/main".into())
    );
    assert_eq!(
        session_base_ref(&json!({"base_ref":"refs/remotes/origin/main"})).unwrap(),
        Some("refs/remotes/origin/main".into())
    );
    for invalid in [
        "main",
        "refs/tags/v1",
        "refs/heads/../main",
        "refs/heads/main^{tree}",
        "refs/remotes/origin/main ",
    ] {
        assert_eq!(
            session_base_ref(&json!({"base_ref":invalid})),
            Err(SessionRuntimeError::InvalidRequest)
        );
    }
    assert_eq!(
        create_semantic_key(CreateOrigin::Direct, "one", None, None, None, None),
        "create:one"
    );
    assert_ne!(
        create_semantic_key(
            CreateOrigin::Direct,
            "one",
            None,
            None,
            None,
            Some("refs/heads/main")
        ),
        create_semantic_key(
            CreateOrigin::Direct,
            "one",
            None,
            None,
            None,
            Some("refs/remotes/origin/main")
        )
    );
}
#[test]
fn rejects_invalid_requests_duplicates_missing_sessions_and_git_failures() {
    let (_tmp, mut runtime) = runtime(FakeSessionGit::fail());
    assert_eq!(
        runtime
            .handle(SessionAction::Create, "bad", &json!({"name":"one"}))
            .unwrap_err(),
        SessionRuntimeError::InvalidOperation
    );
    assert_eq!(
        runtime
            .handle(
                SessionAction::Create,
                &operation(),
                &json!({"name":"../bad"})
            )
            .unwrap_err(),
        SessionRuntimeError::InvalidRequest
    );
    assert_eq!(
        runtime
            .handle(SessionAction::Remove, &operation(), &json!({"name":"none"}))
            .unwrap_err(),
        SessionRuntimeError::UnknownSession
    );
    assert_eq!(
        runtime
            .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
            .unwrap_err(),
        SessionRuntimeError::SessionWorkspaceCreationFailed {
            name: "one".into(),
            detail: "git worktree branch creation failed: no".into(),
        }
    );
    assert_eq!(
        runtime
            .handle(SessionAction::Setup, &operation(), &json!({}))
            .unwrap_err(),
        SessionRuntimeError::InvalidRequest
    );
}

#[test]
fn ambiguous_issue_error_preserves_number_and_sorted_exact_paths() {
    let files = vec![
        PathBuf::from("/repo/.usagi/issues/001-first.md"),
        PathBuf::from("/repo/.usagi/issues/001-second.md"),
    ];
    let error = SessionRuntimeError::AmbiguousIssue(AmbiguousIssueNumber {
        number: 1,
        files: files.clone(),
    });

    assert_eq!(
        error,
        SessionRuntimeError::AmbiguousIssue(AmbiguousIssueNumber { number: 1, files })
    );
    let message = error.safe_message();
    assert!(message.contains("issue #1 is ambiguous"));
    assert!(message.contains("/repo/.usagi/issues/001-first.md"));
    assert!(message.contains("/repo/.usagi/issues/001-second.md"));
}

#[test]
fn role_errors_preserve_their_derived_value_contract() {
    let errors = [
        SessionRuntimeError::RoleConflict(
            Some(RoleId::new("coder").unwrap()),
            Some(RoleId::new("reviewer").unwrap()),
        ),
        SessionRuntimeError::InvalidRole("invalid role".into()),
    ];

    for error in errors {
        let cloned = error.clone();
        assert_eq!(cloned, error);
        assert_eq!(format!("{cloned:?}"), format!("{error:?}"));
        assert!(!error.safe_message().is_empty());
    }
}

#[test]
fn reports_a_reusable_session_name_when_its_branch_already_exists() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    let mut runtime = SessionRuntime::open(
        tmp.path().to_path_buf(),
        &tmp.path().join("daemon"),
        DaemonGeneration::new(),
        BranchExistsGit,
        SystemSessionWorktreeIo,
    )
    .unwrap();

    let error = runtime
        .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
        .unwrap_err();

    assert_eq!(
        error,
        SessionRuntimeError::SessionBranchExists("one".into())
    );
    assert_eq!(
        error.safe_message(),
        "cannot create session \"one\": branch usagi/one already exists; choose a different name or remove the stale branch"
    );
    // The failed reservation is projected so the client can see and remove
    // the name it still owns.
    let listed = runtime.snapshot().unwrap();
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["name"], "one");
    assert_eq!(sessions[0]["lifecycle"], "failed");
    assert_eq!(sessions[0]["failure"]["summary"], error.safe_message());
    assert_eq!(
        runtime.state().unwrap().sessions[0]
            .failure
            .as_ref()
            .unwrap()
            .summary,
        error.safe_message()
    );
}

#[test]
fn reports_a_reusable_session_name_when_its_workspace_already_exists() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    let mut runtime = SessionRuntime::open(
        tmp.path().to_path_buf(),
        &tmp.path().join("daemon"),
        DaemonGeneration::new(),
        WorkspaceExistsGit,
        SystemSessionWorktreeIo,
    )
    .unwrap();

    let error = runtime
        .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
        .unwrap_err();

    assert_eq!(
        error,
        SessionRuntimeError::SessionWorkspaceExists("one".into())
    );
    assert_eq!(
        error.safe_message(),
        "cannot create session \"one\": workspace already exists; choose a different name or remove the stale workspace"
    );
    // The failed reservation is projected so the client can see and remove
    // the name it still owns.
    let listed = runtime.snapshot().unwrap();
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["name"], "one");
    assert_eq!(sessions[0]["lifecycle"], "failed");
    assert_eq!(sessions[0]["failure"]["summary"], error.safe_message());
    assert_eq!(
        runtime.state().unwrap().sessions[0]
            .failure
            .as_ref()
            .unwrap()
            .summary,
        error.safe_message()
    );
}

#[test]
fn lists_a_failed_session_but_refuses_to_resolve_it_then_removes_it_to_free_the_name() {
    let (_tmp, mut runtime) = runtime(FakeSessionGit::ok());
    runtime
        .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
        .unwrap();
    // Force the created session into the Failed lifecycle a real create
    // failure would leave behind: the name stays owned, but the row is not a
    // usable checkout.
    let mut state = runtime.state().unwrap();
    let revision = state.state_revision;
    state.sessions[0].lifecycle = SessionLifecycle::Failed;
    state.sessions[0].failure = Some(Failure {
        stage: FailureStage::Create,
        summary: "create failed".into(),
    });
    runtime.store.replace_if_revision(revision, &state).unwrap();

    // The failed row is projected with its lifecycle and failure summary.
    let listed = runtime.snapshot().unwrap();
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["sessions"][0]["name"], "one");
    assert_eq!(listed["sessions"][0]["lifecycle"], "failed");
    assert_eq!(listed["sessions"][0]["failure"]["summary"], "create failed");

    // Scope resolution still refuses it: attach targets only Available.
    let workspace = state.workspace_id;
    let session_id = state.sessions[0].session_id;
    let worktree_id = state.sessions[0].worktree_id;
    assert_eq!(
        runtime
            .resolve_scope(workspace, session_id, worktree_id)
            .unwrap_err(),
        SessionRuntimeError::ScopeUnavailable
    );

    // Removing the failed row succeeds even though no worktree was created,
    // frees the name, and a same-name create then succeeds.
    let removed = runtime
        .handle(SessionAction::Remove, &operation(), &json!({"name":"one"}))
        .unwrap();
    assert!(removed.body["sessions"].as_array().unwrap().is_empty());
    let recreated = runtime
        .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
        .unwrap();
    assert_eq!(recreated.body["sessions"][0]["name"], "one");
    assert_eq!(recreated.body["sessions"][0]["lifecycle"], "available");
}

#[test]
fn synchronous_failed_session_removal_records_a_branch_deletion_failure() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    let mut runtime = SessionRuntime::open(
        tmp.path().to_path_buf(),
        &tmp.path().join("daemon"),
        DaemonGeneration::new(),
        ScriptedGit::new([
            ScriptedGitResult::Output {
                success: true,
                stdout: "",
                stderr: "",
            },
            ScriptedGitResult::Output {
                success: true,
                stdout: "",
                stderr: "",
            },
            ScriptedGitResult::Output {
                success: true,
                stdout: "",
                stderr: "",
            },
            ScriptedGitResult::Output {
                success: true,
                stdout: "",
                stderr: "",
            },
            ScriptedGitResult::Output {
                success: false,
                stdout: "",
                stderr: "branch is locked",
            },
        ]),
        SystemSessionWorktreeIo,
    )
    .unwrap();
    runtime
        .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
        .unwrap();
    let mut state = runtime.state().unwrap();
    let revision = state.state_revision;
    state.sessions[0].lifecycle = SessionLifecycle::Failed;
    state.sessions[0].failure = Some(Failure {
        stage: FailureStage::Create,
        summary: "create failed".into(),
    });
    runtime.store.replace_if_revision(revision, &state).unwrap();

    let error = runtime
        .handle(SessionAction::Remove, &operation(), &json!({"name":"one"}))
        .unwrap_err();

    assert!(
        matches!(&error, SessionRuntimeError::DurableFailure(summary) if summary.contains("git branch delete failed: branch is locked")),
        "{error:?}"
    );
    let failed = &runtime.state().unwrap().sessions[0];
    assert_eq!(failed.lifecycle, SessionLifecycle::Failed);
    assert_eq!(failed.failure.as_ref().unwrap().stage, FailureStage::Delete);
}

#[test]
fn adopts_a_non_worktree_stale_workspace_before_create_without_invoking_git() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    let stale = tmp.path().join(STATE_DIR).join(SESSIONS_DIR).join("test");
    std::fs::create_dir_all(&stale).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut runtime = SessionRuntime::open(
        tmp.path().to_path_buf(),
        &tmp.path().join("daemon"),
        DaemonGeneration::new(),
        CountingGit {
            calls: Arc::clone(&calls),
        },
        SystemSessionWorktreeIo,
    )
    .unwrap();

    let error = runtime
        .handle(SessionAction::Create, &operation(), &json!({"name":"test"}))
        .unwrap_err();

    assert!(matches!(
        error,
        SessionRuntimeError::OrphanRecoveryBlocked(ref summary)
            if summary.contains("not a registered Git worktree")
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let lifecycle_state = runtime.state().unwrap();
    assert_eq!(lifecycle_state.sessions.len(), 1);
    assert_eq!(
        lifecycle_state.sessions[0].lifecycle,
        SessionLifecycle::Failed
    );
    assert_eq!(
        lifecycle_state.sessions[0].failure.as_ref().unwrap().stage,
        FailureStage::Integrity
    );
    assert!(runtime.state().unwrap().operations.is_empty());
}

#[test]
fn remove_forwards_force_to_the_worktree_removal() {
    let tmp = tempfile::tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = SessionRuntime::open(
        tmp.path().to_path_buf(),
        &tmp.path().join("daemon"),
        DaemonGeneration::new(),
        RecordingGit {
            calls: Arc::clone(&calls),
        },
        SystemSessionWorktreeIo,
    )
    .unwrap();
    runtime
        .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
        .unwrap();
    std::fs::write(
        tmp.path().join(".usagi/sessions/one/.git"),
        "gitdir: /fixture",
    )
    .unwrap();
    runtime
        .handle(
            SessionAction::Remove,
            &operation(),
            &json!({"name":"one", "force":true}),
        )
        .unwrap();

    assert_eq!(
        calls.lock().unwrap()[0].1[..3],
        ["worktree", "remove", "--force"]
    );
}

#[test]
fn remove_rejects_a_non_boolean_force_flag() {
    let (_tmp, mut runtime) = runtime(FakeSessionGit::ok());
    runtime
        .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
        .unwrap();

    assert_eq!(
        runtime
            .handle(
                SessionAction::Remove,
                &operation(),
                &json!({"name":"one", "force":"yes"}),
            )
            .unwrap_err(),
        SessionRuntimeError::InvalidRequest
    );
}

#[test]
fn existing_session_create_is_idempotent_for_the_same_legacy_role() {
    let (tmp, mut runtime) = runtime(FakeSessionGit::ok());
    runtime
        .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
        .unwrap();

    let reply = runtime
        .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
        .unwrap();

    assert_eq!(reply.body["sessions"].as_array().unwrap().len(), 1);
    std::fs::write(
        tmp.path().join(".usagi/roles.toml"),
        r#"version = 1
[roles.coder]
summary = "Implement"
scopes = ["session"]
instructions = "code"
"#,
    )
    .unwrap();
    assert!(matches!(
        runtime.handle(
            SessionAction::Create,
            &operation(),
            &json!({"name":"one", "role":"coder"}),
        ),
        Err(SessionRuntimeError::RoleConflict(None, Some(_)))
    ));
    assert!(matches!(
        runtime.session_role(SessionId::new()),
        Err(SessionRuntimeError::UnknownSession)
    ));
}

#[test]
fn existing_session_create_never_reparents_the_session() {
    let (_tmp, mut runtime) = runtime(FakeSessionGit::ok());
    let original_parent = SessionId::new();
    let different_parent = SessionId::new();
    let created = runtime
        .handle(
            SessionAction::Create,
            &operation(),
            &json!({"name":"one", "parent_session_id":original_parent}),
        )
        .unwrap();
    assert_eq!(
        created.body["sessions"][0]["parent_session_id"],
        json!(original_parent)
    );

    let existing = runtime
        .handle(
            SessionAction::Create,
            &operation(),
            &json!({"name":"one", "parent_session_id":different_parent}),
        )
        .unwrap();

    assert_eq!(existing.body["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(
        existing.body["sessions"][0]["parent_session_id"],
        json!(original_parent)
    );
}

#[test]
fn authenticated_creator_exclusively_owns_the_session_name_and_identity() {
    let (_tmp, mut runtime) = runtime(FakeSessionGit::ok());
    let parent = SessionId::new();
    let creator = AgentId::new();
    let caller = CallerRef {
        session_id: Some(parent),
        agent_id: creator,
    };
    runtime
        .handle(
            SessionAction::Create,
            &operation(),
            &json!({
                "name": "owned",
                "parent_session_id": parent,
                "creator_agent_id": creator,
            }),
        )
        .unwrap();

    let durable = runtime.state().unwrap();
    let session = &durable.sessions[0];
    assert_eq!(session.parent_session_id, Some(parent));
    assert_eq!(session.creator_agent_id, Some(creator));
    assert_eq!(
        runtime.created_session_ids(&caller).unwrap(),
        BTreeSet::from([session.session_id])
    );
    assert_eq!(
        runtime.created_session_id("owned", &caller).unwrap(),
        session.session_id
    );
    assert_eq!(
        runtime.created_session_record_id("owned", &caller).unwrap(),
        session.session_id
    );
    runtime.authorize_create_or_reuse("owned", &caller).unwrap();
    runtime
        .authorize_create_or_reuse("new-name", &caller)
        .unwrap();

    for unrelated in [
        CallerRef {
            session_id: Some(parent),
            agent_id: AgentId::new(),
        },
        CallerRef {
            session_id: Some(SessionId::new()),
            agent_id: creator,
        },
    ] {
        assert!(runtime.created_session_ids(&unrelated).unwrap().is_empty());
        assert_eq!(
            runtime.created_session_id("owned", &unrelated),
            Err(SessionRuntimeError::PermissionDenied)
        );
        assert_eq!(
            runtime.created_session_record_id("owned", &unrelated),
            Err(SessionRuntimeError::PermissionDenied)
        );
        assert_eq!(
            runtime.authorize_create_or_reuse("owned", &unrelated),
            Err(SessionRuntimeError::PermissionDenied)
        );
        assert_eq!(
            runtime.handle(
                SessionAction::Create,
                &operation(),
                &json!({
                    "name": "owned",
                    "parent_session_id": unrelated.session_id,
                    "creator_agent_id": unrelated.agent_id,
                }),
            ),
            Err(SessionRuntimeError::PermissionDenied)
        );
        assert_eq!(
            runtime.handle(
                SessionAction::Remove,
                &operation(),
                &json!({
                    "name": "owned",
                    "parent_session_id": unrelated.session_id,
                    "creator_agent_id": unrelated.agent_id,
                }),
            ),
            Err(SessionRuntimeError::PermissionDenied)
        );
    }
}

#[test]
fn creator_metadata_is_validated_and_failed_records_are_removal_only() {
    let (_failed_tmp, mut failed_runtime) = runtime(FakeSessionGit::fail());
    let (_tmp, mut runtime) = runtime(FakeSessionGit::ok());
    let parent = SessionId::new();
    let creator = AgentId::new();
    let caller = CallerRef {
        session_id: Some(parent),
        agent_id: creator,
    };
    runtime
        .handle(
            SessionAction::Create,
            &operation(),
            &json!({
                "name": "owned",
                "parent_session_id": parent,
                "creator_agent_id": creator,
            }),
        )
        .unwrap();
    assert_eq!(
        runtime.created_session_id("missing", &caller),
        Err(SessionRuntimeError::UnknownSession)
    );
    assert_eq!(
        runtime.created_session_record_id("missing", &caller),
        Err(SessionRuntimeError::UnknownSession)
    );
    assert_eq!(
        runtime.handle(
            SessionAction::Remove,
            &operation(),
            &json!({
                "name": "missing",
                "parent_session_id": parent,
                "creator_agent_id": creator,
            }),
        ),
        Err(SessionRuntimeError::UnknownSession)
    );
    assert!(
        runtime.snapshot().unwrap()["sessions"][0]
            .get("creator_agent_id")
            .is_none()
    );
    assert!(matches!(
        failed_runtime.handle(
            SessionAction::Create,
            &operation(),
            &json!({
                "name": "failed-owned",
                "parent_session_id": parent,
                "creator_agent_id": creator,
            }),
        ),
        Err(SessionRuntimeError::SessionWorkspaceCreationFailed { .. })
    ));
    assert_eq!(
        failed_runtime.created_session_id("failed-owned", &caller),
        Err(SessionRuntimeError::UnknownSession)
    );
    assert!(
        failed_runtime
            .created_session_record_id("failed-owned", &caller)
            .is_ok()
    );
    runtime
        .handle(
            SessionAction::Remove,
            &operation(),
            &json!({
                "name": "owned",
                "parent_session_id": parent,
                "creator_agent_id": creator,
            }),
        )
        .unwrap();
    assert!(runtime.created_session_ids(&caller).unwrap().is_empty());
}

#[test]
fn creator_metadata_and_base_ref_validation_fail_before_effect() {
    let (_tmp, mut runtime) = runtime(FakeSessionGit::ok());
    let parent = SessionId::new();
    let creator = AgentId::new();
    for payload in [
        json!({"name":"invalid-creator", "creator_agent_id":"not-a-resource-id"}),
        json!({
            "name": "invalid-base-ref",
            "base_ref": "main",
            "parent_session_id": parent,
            "creator_agent_id": creator,
        }),
    ] {
        assert_eq!(
            runtime.handle(SessionAction::Create, &operation(), &payload),
            Err(SessionRuntimeError::InvalidRequest)
        );
    }
    assert_eq!(
        runtime.handle(
            SessionAction::Remove,
            &operation(),
            &json!({"name":"missing", "creator_agent_id":"not-a-resource-id"}),
        ),
        Err(SessionRuntimeError::InvalidRequest)
    );
}

#[test]
fn creator_authorization_reports_unreadable_state() {
    let (tmp, runtime) = runtime(FakeSessionGit::ok());
    let caller = CallerRef {
        session_id: Some(SessionId::new()),
        agent_id: AgentId::new(),
    };
    std::fs::write(tmp.path().join("daemon/sessions.json"), "not json").unwrap();

    assert_eq!(
        runtime.created_session_ids(&caller),
        Err(SessionRuntimeError::Storage)
    );
    assert_eq!(
        runtime.created_session_id("owned", &caller),
        Err(SessionRuntimeError::Storage)
    );
    assert_eq!(
        runtime.created_session_record_id("owned", &caller),
        Err(SessionRuntimeError::Storage)
    );
    assert_eq!(
        runtime.authorize_create_or_reuse("owned", &caller),
        Err(SessionRuntimeError::Storage)
    );
}

#[test]
fn catalog_default_assignment_is_stable_and_conflicting_role_is_rejected() {
    let (tmp, mut runtime) = runtime(FakeSessionGit::ok());
    std::fs::write(
        tmp.path().join(".usagi/roles.toml"),
        r#"version = 1
[defaults]
session = "coder"
[roles.coder]
summary = "Implement"
scopes = ["session"]
instructions = "code"
[roles.reviewer]
summary = "Review"
scopes = ["session"]
instructions = "review"
"#,
    )
    .unwrap();

    let created = runtime
        .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
        .unwrap();
    assert_eq!(created.body["sessions"][0]["role_id"], "coder");
    let replay = runtime
        .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
        .unwrap();
    assert_eq!(replay.body["sessions"].as_array().unwrap().len(), 1);
    assert!(matches!(
        runtime.handle(
            SessionAction::Create,
            &operation(),
            &json!({"name":"one", "role":"reviewer"}),
        ),
        Err(SessionRuntimeError::RoleConflict(..))
    ));
    assert!(matches!(
        runtime.handle(
            SessionAction::Create,
            &operation(),
            &json!({"name":"one", "role":"missing"}),
        ),
        Err(SessionRuntimeError::InvalidRole(_))
    ));
    let id = runtime.session_id("one").unwrap();
    assert_eq!(runtime.session_role(id).unwrap().unwrap().as_str(), "coder");

    std::fs::write(
        tmp.path().join(".usagi/roles.toml"),
        r#"version = 1
[defaults]
session = "coder"
[roles.coder]
summary = "Changed summary"
scopes = ["session"]
instructions = "changed"
"#,
    )
    .unwrap();
    let snapshot = runtime.snapshot().unwrap();
    assert_eq!(snapshot["sessions"][0]["role_id"], "coder");
    assert_eq!(snapshot["sessions"][0]["role_summary"], "Changed summary");
    let status = runtime
        .handle(SessionAction::Status, &operation(), &json!({}))
        .unwrap();
    assert_eq!(status.body["sessions"][0]["role_id"], "coder");
    assert_eq!(
        status.body["sessions"][0]["role_summary"],
        "Changed summary"
    );

    assert_eq!(
        runtime
            .handle(
                SessionAction::Create,
                &operation(),
                &json!({"name":"invalid", "role":"Bad"}),
            )
            .unwrap_err(),
        SessionRuntimeError::InvalidRequest
    );

    std::fs::write(
        tmp.path().join(".usagi/roles.toml"),
        r#"version = 1
[roles.reviewer]
summary = "Review"
scopes = ["session"]
instructions = "review"
"#,
    )
    .unwrap();
    assert!(matches!(
        runtime.handle(SessionAction::Create, &operation(), &json!({"name":"one"})),
        Err(SessionRuntimeError::InvalidRole(_))
    ));
}

#[test]
fn malformed_catalog_fails_create_before_git_effect() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".usagi")).unwrap();
    std::fs::write(tmp.path().join(".usagi/roles.toml"), "version = 99\n").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut runtime = SessionRuntime::open(
        tmp.path().to_path_buf(),
        &tmp.path().join("daemon"),
        DaemonGeneration::new(),
        FakeSessionGit::ok(),
        FakeSessionWorktreeIo {
            occupied: false,
            build_calls: Arc::clone(&calls),
        },
    )
    .unwrap();
    assert!(matches!(
        runtime.handle(SessionAction::Create, &operation(), &json!({"name":"one"})),
        Err(SessionRuntimeError::InvalidRole(_))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    std::fs::write(
        tmp.path().join(".usagi/roles.toml"),
        r#"version = 1
[roles.director]
summary = "Direct"
scopes = ["root"]
instructions = "direct"
"#,
    )
    .unwrap();
    assert!(matches!(
        runtime.handle(
            SessionAction::Create,
            &operation(),
            &json!({"name":"one", "role":"director"}),
        ),
        Err(SessionRuntimeError::InvalidRole(_))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn effective_role_catalog_rejects_a_malformed_catalog() {
    let (tmp, runtime) = runtime(FakeSessionGit::ok());
    std::fs::write(tmp.path().join(".usagi/roles.toml"), "version = 99\n").unwrap();

    assert!(matches!(
        runtime.effective_role_catalog(),
        Err(SessionRuntimeError::InvalidRole(message))
            if message == "effective role catalog is invalid"
    ));
}

#[test]
fn worktree_failure_detail_is_single_line_bounded_and_nonempty() {
    assert_eq!(
        worktree_failure_detail("git worktree add failed: fatal: first\nsecond"),
        "fatal: first"
    );
    assert_eq!(
        worktree_failure_detail("\n\t"),
        "Git rejected workspace creation"
    );
    assert_eq!(
        worktree_failure_detail(&"x".repeat(200)).chars().count(),
        160
    );
}
#[test]
fn operation_id_is_idempotent_only_for_the_same_semantic_request() {
    let (_tmp, mut runtime) = runtime(FakeSessionGit::ok());
    let operation = operation();
    runtime
        .handle(SessionAction::Create, &operation, &json!({"name":"one"}))
        .unwrap();
    assert!(
        runtime
            .handle(SessionAction::Create, &operation, &json!({"name":"one"}))
            .is_ok()
    );
    assert_eq!(
        runtime
            .handle(SessionAction::Create, &operation, &json!({"name":"two"}))
            .unwrap_err(),
        SessionRuntimeError::IdempotencyConflict
    );
}

#[test]
fn replaying_a_successful_create_after_daemon_restart_does_not_create_twice() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    let operation = operation();
    let first_calls = Arc::new(AtomicUsize::new(0));
    let mut first = SessionRuntime::open(
        tmp.path().to_path_buf(),
        &tmp.path().join("daemon"),
        DaemonGeneration::new(),
        CountingGit {
            calls: Arc::clone(&first_calls),
        },
        SystemSessionWorktreeIo,
    )
    .unwrap();

    let created = first
        .handle(SessionAction::Create, &operation, &json!({"name":"one"}))
        .unwrap();
    // Resolve + attribute scan + branch + metadata + config + checkout. The
    // replay below performs none of them.
    assert_eq!(first_calls.load(Ordering::SeqCst), 6);
    drop(first);

    let replay_calls = Arc::new(AtomicUsize::new(0));
    let mut restarted = SessionRuntime::open(
        tmp.path().to_path_buf(),
        &tmp.path().join("daemon"),
        DaemonGeneration::new(),
        CountingGit {
            calls: Arc::clone(&replay_calls),
        },
        SystemSessionWorktreeIo,
    )
    .unwrap();
    let replayed = restarted
        .handle(SessionAction::Create, &operation, &json!({"name":"one"}))
        .unwrap();

    assert_eq!(replayed.body, created.body);
    assert_eq!(replay_calls.load(Ordering::SeqCst), 0);
    assert_eq!(replayed.body["sessions"].as_array().unwrap().len(), 1);
}

#[test]
fn failed_create_replays_the_same_failure_without_repeating_the_effect() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    let state_dir = tmp.path().join("daemon");
    let operation = operation();
    let first_calls = Arc::new(AtomicUsize::new(0));
    let mut first = SessionRuntime::open(
        tmp.path().to_path_buf(),
        &state_dir,
        DaemonGeneration::new(),
        OutcomeGit {
            succeeds: false,
            calls: Arc::clone(&first_calls),
        },
        SystemSessionWorktreeIo,
    )
    .unwrap();

    let failed = first
        .handle(SessionAction::Create, &operation, &json!({"name":"one"}))
        .unwrap_err();
    let replayed = first
        .handle(SessionAction::Create, &operation, &json!({"name":"one"}))
        .unwrap_err();
    assert_eq!(replayed.safe_message(), failed.safe_message());
    assert_eq!(
        first
            .handle(SessionAction::Create, &operation, &json!({"name":"two"}))
            .unwrap_err(),
        SessionRuntimeError::IdempotencyConflict
    );
    // Resolve + attribute scan + failed branch creation. The replay above
    // performs none of them.
    assert_eq!(first_calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        first.state().unwrap().operations[0].status,
        OperationStatus::Failed
    );
    drop(first);

    let restart_calls = Arc::new(AtomicUsize::new(0));
    let mut restarted = SessionRuntime::open(
        tmp.path().to_path_buf(),
        &state_dir,
        DaemonGeneration::new(),
        OutcomeGit {
            succeeds: true,
            calls: Arc::clone(&restart_calls),
        },
        SystemSessionWorktreeIo,
    )
    .unwrap();
    let reopened = restarted
        .handle(SessionAction::Create, &operation, &json!({"name":"one"}))
        .unwrap_err();
    assert_eq!(reopened.safe_message(), failed.safe_message());
    assert_eq!(restart_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn failed_remove_replays_the_same_failure_without_repeating_the_effect() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("daemon");
    let create_calls = Arc::new(AtomicUsize::new(0));
    let mut creator = SessionRuntime::open(
        tmp.path().to_path_buf(),
        &state_dir,
        DaemonGeneration::new(),
        OutcomeGit {
            succeeds: true,
            calls: Arc::clone(&create_calls),
        },
        SystemSessionWorktreeIo,
    )
    .unwrap();
    creator
        .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
        .unwrap();
    let session_root = tmp.path().join(STATE_DIR).join(SESSIONS_DIR).join("one");
    std::fs::create_dir_all(&session_root).unwrap();
    std::fs::write(session_root.join(".git"), "gitdir: /fixture").unwrap();
    drop(creator);

    let operation = operation();
    let first_calls = Arc::new(AtomicUsize::new(0));
    let mut first = SessionRuntime::open(
        tmp.path().to_path_buf(),
        &state_dir,
        DaemonGeneration::new(),
        OutcomeGit {
            succeeds: false,
            calls: Arc::clone(&first_calls),
        },
        SystemSessionWorktreeIo,
    )
    .unwrap();
    let failed = first
        .handle(
            SessionAction::Remove,
            &operation,
            &json!({"name":"one", "force":true}),
        )
        .unwrap_err();
    let replayed = first
        .handle(
            SessionAction::Remove,
            &operation,
            &json!({"name":"one", "force":true}),
        )
        .unwrap_err();
    assert_eq!(replayed.safe_message(), failed.safe_message());
    assert_eq!(
        first
            .handle(
                SessionAction::Remove,
                &operation,
                &json!({"name":"one", "force":false}),
            )
            .unwrap_err(),
        SessionRuntimeError::IdempotencyConflict
    );
    assert_eq!(first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        first.state().unwrap().operations[1].status,
        OperationStatus::Failed
    );
    drop(first);

    let restart_calls = Arc::new(AtomicUsize::new(0));
    let mut restarted = SessionRuntime::open(
        tmp.path().to_path_buf(),
        &state_dir,
        DaemonGeneration::new(),
        OutcomeGit {
            succeeds: true,
            calls: Arc::clone(&restart_calls),
        },
        SystemSessionWorktreeIo,
    )
    .unwrap();
    let reopened = restarted
        .handle(
            SessionAction::Remove,
            &operation,
            &json!({"name":"one", "force":true}),
        )
        .unwrap_err();
    assert_eq!(reopened.safe_message(), failed.safe_message());
    assert_eq!(
        restarted
            .handle(
                SessionAction::Remove,
                &operation,
                &json!({"name":"one", "force":false}),
            )
            .unwrap_err(),
        SessionRuntimeError::IdempotencyConflict
    );
    assert_eq!(restart_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn resolver_requires_complete_available_scope_and_restart_reconciles_interrupted_work() {
    let (tmp, mut runtime) = runtime(FakeSessionGit::ok());
    let created = runtime
        .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
        .unwrap();
    let session = created.body["sessions"][0].clone();
    let workspace = serde_json::from_value(created.body["workspace_id"].clone()).unwrap();
    let session_id = serde_json::from_value(session["session_id"].clone()).unwrap();
    let worktree_id = serde_json::from_value(session["worktree_id"].clone()).unwrap();
    assert!(
        runtime
            .resolve_scope(workspace, session_id, worktree_id)
            .is_ok()
    );
    assert_eq!(
        runtime
            .resolve_scope(WorkspaceId::new(), session_id, worktree_id)
            .unwrap_err(),
        SessionRuntimeError::ScopeUnavailable
    );

    let operation = OperationId::new();
    runtime
        .store
        .apply(
            runtime.generation,
            LifecycleEvent::ReserveCreate {
                name: "interrupted".into(),
                role_id: None,
                parent_session_id: None,
                creator_agent_id: None,
                operation: journal(
                    operation,
                    runtime.generation,
                    semantic_key(SessionAction::Create, "interrupted"),
                ),
            },
            Utc::now(),
        )
        .unwrap();
    let mut restarted = SessionRuntime::open(
        tmp.path().to_path_buf(),
        &tmp.path().join("daemon"),
        DaemonGeneration::new(),
        FakeSessionGit::ok(),
        SystemSessionWorktreeIo,
    )
    .unwrap();
    // Both the completed `Available` session and the interrupted work,
    // reconciled to `Failed` on restart, are projected to the client.
    let snapshot = restarted.snapshot().unwrap();
    let listed = snapshot["sessions"].as_array().unwrap();
    assert_eq!(listed.len(), 2);
    let interrupted = listed
        .iter()
        .find(|session| session["name"] == "interrupted")
        .unwrap();
    assert_eq!(interrupted["lifecycle"], "failed");
    assert_eq!(
        interrupted["failure"]["summary"],
        "interrupted; explicit recovery required"
    );
    assert_eq!(
        restarted.state().unwrap().sessions[1]
            .failure
            .as_ref()
            .unwrap()
            .summary,
        "interrupted; explicit recovery required"
    );
    assert_eq!(
        restarted.state().unwrap().operations[1].status,
        OperationStatus::Failed
    );
    assert_eq!(
        restarted
            .handle(
                SessionAction::Create,
                &operation.to_string(),
                &json!({"name":"interrupted"})
            )
            .unwrap_err()
            .safe_message(),
        "interrupted; explicit recovery required"
    );
}

#[test]
fn open_repairs_a_legacy_failed_session_and_replays_failure() {
    let (tmp, mut runtime) = runtime(FakeSessionGit::ok());
    let operation = operation();
    runtime
        .handle(SessionAction::Create, &operation, &json!({"name":"legacy"}))
        .unwrap();
    let mut legacy = runtime.state().unwrap();
    let revision = legacy.state_revision;
    legacy.sessions[0].lifecycle = SessionLifecycle::Failed;
    legacy.sessions[0].failure = Some(Failure {
        stage: FailureStage::Create,
        summary: "legacy create failed".into(),
    });
    legacy.sessions[0].operation_id = None;
    legacy.operations[0].status = OperationStatus::Succeeded;
    runtime
        .store
        .replace_if_revision(revision, &legacy)
        .unwrap();
    drop(runtime);

    let mut reopened = SessionRuntime::open(
        tmp.path().to_path_buf(),
        &tmp.path().join("daemon"),
        DaemonGeneration::new(),
        FakeSessionGit::ok(),
        SystemSessionWorktreeIo,
    )
    .unwrap();
    let repaired = reopened.state().unwrap();
    assert_eq!(repaired.operations[0].status, OperationStatus::Failed);
    assert_eq!(
        repaired.sessions[0].operation_id,
        Some(repaired.operations[0].operation_id)
    );
    assert_eq!(
        reopened
            .handle(SessionAction::Create, &operation, &json!({"name":"legacy"}))
            .unwrap_err()
            .safe_message(),
        "legacy create failed"
    );
}

#[test]
fn restart_from_another_directory_uses_the_shared_session_state_and_root() {
    let tmp = tempfile::tempdir().unwrap();
    let original_root = tmp.path().join("original");
    let another_directory = tmp.path().join("another");
    let state_dir = tmp.path().join("shared-daemon");
    std::fs::create_dir_all(&original_root).unwrap();
    std::fs::create_dir_all(&another_directory).unwrap();

    let mut first = SessionRuntime::open(
        original_root.clone(),
        &state_dir,
        DaemonGeneration::new(),
        FakeSessionGit::ok(),
        SystemSessionWorktreeIo,
    )
    .unwrap();
    first
        .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
        .unwrap();
    drop(first);

    let restarted = SessionRuntime::open(
        another_directory,
        &state_dir,
        DaemonGeneration::new(),
        FakeSessionGit::ok(),
        SystemSessionWorktreeIo,
    )
    .unwrap();

    assert_eq!(restarted.repository_root(), original_root);
    assert_eq!(
        restarted.snapshot().unwrap()["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn first_shared_start_migrates_legacy_repository_session_state() {
    let tmp = tempfile::tempdir().unwrap();
    let repository = tmp.path().join("repository");
    let legacy_dir = project_data_dir(&repository);
    let state_dir = tmp.path().join("shared-daemon");
    let mut legacy = WorkspaceLifecycleState::new(WorkspaceId::new(), Utc::now());
    legacy.sessions.push(ManagedSession::new_creating(
        "legacy".into(),
        OperationId::new(),
        Utc::now(),
    ));
    json_file::write_atomic(
        &legacy_dir,
        &legacy_dir.join("lifecycle-state.json"),
        &legacy,
    )
    .unwrap();

    let migrated = SessionRuntime::open(
        repository.clone(),
        &state_dir,
        DaemonGeneration::new(),
        FakeSessionGit::ok(),
        SystemSessionWorktreeIo,
    )
    .unwrap();

    assert_eq!(migrated.repo_root, repository);
    assert_eq!(migrated.state().unwrap().sessions[0].name, "legacy");
    // The interrupted `Creating` reservation is reconciled to `Failed` on
    // open and then projected, so the migrated name is visible and removable.
    let listed = migrated.snapshot().unwrap();
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["name"], "legacy");
    assert_eq!(sessions[0]["lifecycle"], "failed");
    assert!(state_dir.join("sessions.json").is_file());
    assert!(!legacy_dir.join("lifecycle-state.json").exists());
}

#[test]
fn create_recursively_mirrors_plain_entries_and_adds_a_worktree_per_nested_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    let destination = workspace.join(".usagi/sessions/feature");
    let nested_repo = workspace.join("services/api");
    std::fs::create_dir_all(nested_repo.join(".git")).unwrap();
    std::fs::create_dir_all(workspace.join("docs")).unwrap();
    std::fs::write(workspace.join("README.md"), "read me").unwrap();
    std::fs::write(workspace.join("docs/guide.md"), "guide").unwrap();

    let git = RecordingGit::new();
    SystemSessionWorktreeIo
        .build_session_tree(
            &git,
            &workspace,
            &destination,
            "usagi/feature",
            Some("refs/remotes/origin/main"),
        )
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(destination.join("README.md")).unwrap(),
        "read me"
    );
    assert_eq!(
        std::fs::read_to_string(destination.join("docs/guide.md")).unwrap(),
        "guide"
    );
    let calls = git.calls.lock().unwrap();
    assert_eq!(calls.len(), 6);
    assert_eq!(
        calls.first().map(|(_, args)| args.as_slice()),
        Some(
            [
                "rev-parse".into(),
                "--verify".into(),
                "--end-of-options".into(),
                "refs/remotes/origin/main^{commit}".into(),
            ]
            .as_slice()
        )
    );
    assert_eq!(
        calls.get(3),
        Some(&(
            nested_repo,
            vec![
                "worktree".into(),
                "add".into(),
                "--no-checkout".into(),
                "--".into(),
                destination
                    .join("services/api")
                    .to_string_lossy()
                    .into_owned(),
                "usagi/feature".into(),
            ],
        ))
    );
}

#[test]
fn opening_a_repository_migrates_v1_usagi_ignore_rules() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    std::fs::write(
        tmp.path().join(".gitignore"),
        "target\n.usagi/*\n!.usagi/issues/\n.usagi/issues/index.json\n",
    )
    .unwrap();

    let _runtime = SessionRuntime::open(
        tmp.path().to_path_buf(),
        &tmp.path().join("daemon"),
        DaemonGeneration::new(),
        FakeSessionGit::ok(),
        SystemSessionWorktreeIo,
    )
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(tmp.path().join(".usagi/.gitignore")).unwrap(),
        usagi_core::infrastructure::gitignore::USAGI_GITIGNORE
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap(),
        "target\n"
    );
}

#[test]
fn bound_workspace_root_predicts_the_root_open_binds() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("daemon");
    let first = tmp.path().join("first");
    let second = tmp.path().join("second");
    std::fs::create_dir_all(&first).unwrap();
    std::fs::create_dir_all(&second).unwrap();

    // Fresh state: the prediction is the startup candidate, and `open` binds
    // exactly that.
    assert_eq!(
        SessionRuntime::bound_workspace_root(&state_dir, first.clone()).unwrap(),
        first
    );
    let runtime = SessionRuntime::open(
        first.clone(),
        &state_dir,
        DaemonGeneration::new(),
        FakeSessionGit::ok(),
        SystemSessionWorktreeIo,
    )
    .unwrap();
    assert_eq!(runtime.repository_root(), first);
    drop(runtime);

    // Durable state: the stored root wins over a different candidate for the
    // prediction and for `open` alike, so the fence cannot key a workspace
    // the runtime will not own.
    assert_eq!(
        SessionRuntime::bound_workspace_root(&state_dir, second.clone()).unwrap(),
        first
    );
    let reopened = SessionRuntime::open(
        second,
        &state_dir,
        DaemonGeneration::new(),
        FakeSessionGit::ok(),
        SystemSessionWorktreeIo,
    )
    .unwrap();
    assert_eq!(reopened.repository_root(), first);
}

#[test]
fn bound_workspace_root_reports_unreadable_state() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("daemon");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::write(state_dir.join("sessions.json"), "not json").unwrap();
    assert_eq!(
        SessionRuntime::bound_workspace_root(&state_dir, tmp.path().to_path_buf()),
        Err(SessionRuntimeError::Storage)
    );
}

#[test]
fn session_id_reports_unreadable_state() {
    let (tmp, mut runtime) = runtime(FakeSessionGit::ok());
    runtime
        .handle(SessionAction::Create, &operation(), &json!({"name": "one"}))
        .unwrap();
    let session_id = runtime.session_id("one").unwrap();
    assert!(runtime.session_scope_by_id(session_id).is_ok());
    assert_eq!(
        runtime.session_id("missing"),
        Err(SessionRuntimeError::UnknownSession)
    );
    std::fs::write(tmp.path().join("daemon/sessions.json"), "not json").unwrap();

    assert_eq!(runtime.session_id("one"), Err(SessionRuntimeError::Storage));
}

#[test]
fn pending_teardowns_skip_incomplete_delete_records() {
    let (_tmp, runtime) = runtime(FakeSessionGit::ok());
    let mut missing_plan =
        ManagedSession::new_creating("missing-plan".into(), OperationId::new(), Utc::now());
    missing_plan.lifecycle = SessionLifecycle::Deleting;
    let mut missing_operation =
        ManagedSession::new_creating("missing-operation".into(), OperationId::new(), Utc::now());
    missing_operation.lifecycle = SessionLifecycle::Deleting;
    missing_operation.delete_plan = Some(DeletePlan {
        targets: vec!["missing-operation".into()],
        force: false,
        delete_branch: false,
        branch_name: None,
        force_delete_branch: false,
        merged_head_oid: None,
    });
    missing_operation.operation_id = None;

    let mut state = runtime.state().unwrap();
    let revision = state.state_revision;
    state.state_revision += 1;
    state.sessions = vec![missing_plan, missing_operation];
    runtime.store.replace_if_revision(revision, &state).unwrap();

    assert!(runtime.pending_teardowns().unwrap().is_empty());
}

/// A Git runner that records whether the shared session lock was free at the
/// moment Git ran. `perform_create`/`perform_remove` must release the lock
/// before invoking Git, so a same-thread `try_lock` succeeds here.
struct LockProbeGit {
    runtime: std::sync::Weak<Mutex<SessionRuntime>>,
    observed_unlocked: Arc<std::sync::atomic::AtomicBool>,
}
impl GitRunner for LockProbeGit {
    fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
        let runtime = self.runtime.upgrade().expect("runtime remains alive");
        self.observed_unlocked.store(
            runtime.try_lock().is_ok(),
            std::sync::atomic::Ordering::SeqCst,
        );
        Ok(checkout_validation_output(args).unwrap_or(GitOutput {
            success: true,
            stdout: String::new(),
            stderr: String::new(),
        }))
    }
}

/// A Git runner that poisons the shared session lock while it runs, so the
/// `finish_*` re-lock inside `perform_*` observes a poisoned lock.
struct PoisoningGit {
    runtime: std::sync::Weak<Mutex<SessionRuntime>>,
}
impl GitRunner for PoisoningGit {
    fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
        if let Some(output) = checkout_validation_output(args) {
            return Ok(output);
        }
        if let Some(runtime) = self.runtime.upgrade() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = runtime.lock().unwrap();
                panic!("poison the session lock mid Git effect");
            }));
        }
        Ok(GitOutput {
            success: true,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

fn poison_lock(runtime: &Arc<Mutex<SessionRuntime>>) {
    let clone = Arc::clone(runtime);
    let _ = std::thread::spawn(move || {
        let _guard = clone.lock().unwrap();
        panic!("poison the session lock before begin");
    })
    .join();
}

#[test]
fn perform_create_releases_the_session_lock_while_building_the_worktree() {
    let (_tmp, rt) = runtime(FakeSessionGit::ok());
    let runtime = Arc::new(Mutex::new(rt));
    let observed_unlocked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let git = LockProbeGit {
        runtime: Arc::downgrade(&runtime),
        observed_unlocked: Arc::clone(&observed_unlocked),
    };
    let reply = perform_create(&runtime, &git, &operation(), &json!({"name":"one"})).unwrap();
    assert!(
        observed_unlocked.load(std::sync::atomic::Ordering::SeqCst),
        "the session lock must be released while `git worktree add` runs"
    );
    assert_eq!(reply.body["sessions"][0]["name"], "one");
}

#[test]
fn configured_setup_commands_run_in_order_without_holding_the_session_lock() {
    let temporary = tempfile::tempdir().unwrap();
    let repository = temporary.path().join("repository");
    std::fs::create_dir_all(repository.join(".usagi")).unwrap();
    std::fs::write(
        repository.join(".usagi/config.toml"),
        "[session]\nsetup_commands = [\"first\", \"  \", \"second\"]\n",
    )
    .unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runtime_probe = Arc::new(Mutex::new(None));
    let observed_unlocked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let runtime = Arc::new(Mutex::new(
        SessionRuntime::open(
            repository.clone(),
            &temporary.path().join("daemon"),
            DaemonGeneration::new(),
            FakeSessionGit::ok(),
            SetupSessionWorktreeIo {
                calls: Arc::clone(&calls),
                fail_on: None,
                runtime: Arc::clone(&runtime_probe),
                observed_unlocked: Arc::clone(&observed_unlocked),
            },
        )
        .unwrap(),
    ));
    *runtime_probe.lock().unwrap() = Some(Arc::downgrade(&runtime));

    let reply = perform_create(
        &runtime,
        &FakeSessionGit::ok(),
        &operation(),
        &json!({"name":"configured"}),
    )
    .unwrap();

    let session_root = repository.join(".usagi/sessions/configured");
    assert_eq!(
        *calls.lock().unwrap(),
        [
            (session_root.clone(), "first".into()),
            (session_root, "second".into())
        ]
    );
    assert!(observed_unlocked.load(Ordering::SeqCst));
    assert_eq!(reply.body["sessions"][0]["lifecycle"], "available");
    assert!(reply.body["sessions"][0].get("setup_plan").is_none());
}

#[test]
fn create_and_setup_completion_refresh_the_fence_after_unrelated_mutations() {
    let temporary = tempfile::tempdir().unwrap();
    let repository = temporary.path().join("repository");
    std::fs::create_dir_all(repository.join(".usagi")).unwrap();
    std::fs::write(
        repository.join(".usagi/config.toml"),
        "[session]\nsetup_commands = [\"configured\"]\n",
    )
    .unwrap();
    let mut runtime = SessionRuntime::open(
        repository,
        &temporary.path().join("daemon"),
        DaemonGeneration::new(),
        FakeSessionGit::ok(),
        SetupSessionWorktreeIo {
            calls: Arc::new(Mutex::new(Vec::new())),
            fail_on: None,
            runtime: Arc::new(Mutex::new(None)),
            observed_unlocked: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        },
    )
    .unwrap();

    let first = pending_create(
        runtime
            .begin_create(CreateOrigin::Direct, &operation(), &json!({"name":"first"}))
            .unwrap(),
    )
    .unwrap();
    // Completing another session advances the workspace revision after the
    // first create captured its admission fence.
    runtime
        .handle(
            SessionAction::Create,
            &operation(),
            &json!({"name":"during-create"}),
        )
        .unwrap();
    let initializing = pending_initialize(runtime.finish_create(*first, Ok(())).unwrap()).unwrap();

    // The setup fence must likewise survive an unrelated lifecycle change
    // while the configured command is running.
    runtime
        .handle(
            SessionAction::Create,
            &operation(),
            &json!({"name":"during-setup"}),
        )
        .unwrap();
    runtime.finish_initialize(initializing, Ok(())).unwrap();

    let state = runtime.state().unwrap();
    assert_eq!(state.sessions.len(), 3);
    assert!(
        state
            .sessions
            .iter()
            .all(|session| session.lifecycle == SessionLifecycle::Available)
    );
}

#[test]
fn create_and_setup_completion_rejects_a_different_workspace_identity() {
    let temporary = tempfile::tempdir().unwrap();
    let repository = temporary.path().join("repository");
    std::fs::create_dir_all(repository.join(".usagi")).unwrap();
    std::fs::write(
        repository.join(".usagi/config.toml"),
        "[session]\nsetup_commands = [\"configured\"]\n",
    )
    .unwrap();
    let mut runtime = SessionRuntime::open(
        repository,
        &temporary.path().join("daemon"),
        DaemonGeneration::new(),
        FakeSessionGit::ok(),
        SetupSessionWorktreeIo {
            calls: Arc::new(Mutex::new(Vec::new())),
            fail_on: None,
            runtime: Arc::new(Mutex::new(None)),
            observed_unlocked: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        },
    )
    .unwrap();
    let create = pending_create(
        runtime
            .begin_create(
                CreateOrigin::Direct,
                &operation(),
                &json!({"name":"create"}),
            )
            .unwrap(),
    )
    .unwrap();
    let admitted_workspace_id = create.fence.workspace_id;
    let mut state = runtime.state().unwrap();
    let revision = state.state_revision;
    state.workspace_id = WorkspaceId::new();
    runtime.store.replace_if_revision(revision, &state).unwrap();
    assert!(matches!(
        runtime.finish_create(*create, Ok(())),
        Err(SessionRuntimeError::Storage)
    ));

    let mut state = runtime.state().unwrap();
    let revision = state.state_revision;
    state.workspace_id = admitted_workspace_id;
    runtime.store.replace_if_revision(revision, &state).unwrap();
    let create = pending_create(
        runtime
            .begin_create(
                CreateOrigin::Direct,
                &operation(),
                &json!({"name":"initialize"}),
            )
            .unwrap(),
    )
    .unwrap();
    let initialize = pending_initialize(runtime.finish_create(*create, Ok(())).unwrap()).unwrap();
    let mut state = runtime.state().unwrap();
    let revision = state.state_revision;
    state.workspace_id = WorkspaceId::new();
    runtime.store.replace_if_revision(revision, &state).unwrap();
    assert_eq!(
        runtime.finish_initialize(initialize, Ok(())).unwrap_err(),
        SessionRuntimeError::Storage
    );
}

#[test]
fn setup_failure_is_durable_and_does_not_skip_later_commands_or_replay() {
    let temporary = tempfile::tempdir().unwrap();
    let repository = temporary.path().join("repository");
    std::fs::create_dir_all(repository.join(".usagi")).unwrap();
    std::fs::write(
        repository.join(".usagi/config.toml"),
        "[session]\nsetup_commands = [\"fail\", \"fail\", \"after\"]\n",
    )
    .unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runtime = Arc::new(Mutex::new(
        SessionRuntime::open(
            repository,
            &temporary.path().join("daemon"),
            DaemonGeneration::new(),
            FakeSessionGit::ok(),
            SetupSessionWorktreeIo {
                calls: Arc::clone(&calls),
                fail_on: Some("fail".into()),
                runtime: Arc::new(Mutex::new(None)),
                observed_unlocked: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        )
        .unwrap(),
    ));
    let operation = operation();

    let error = perform_create(
        &runtime,
        &FakeSessionGit::ok(),
        &operation,
        &json!({"name":"failed-setup"}),
    )
    .unwrap_err();
    assert_eq!(
        error,
        SessionRuntimeError::DurableFailure(
            "cannot initialize session \"failed-setup\": setup command 1 failed".into()
        )
    );
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .map(|(_, command)| command.as_str())
            .collect::<Vec<_>>(),
        ["fail", "fail", "after"]
    );
    let state = runtime.lock().unwrap().state().unwrap();
    assert_eq!(state.sessions[0].lifecycle, SessionLifecycle::Failed);
    assert_eq!(
        state.sessions[0].failure.as_ref().unwrap().stage,
        FailureStage::Initialize
    );
    assert_eq!(
        state.sessions[0].setup_plan.as_ref().unwrap().commands,
        ["fail", "fail", "after"]
    );
    assert!(
        runtime.lock().unwrap().snapshot().unwrap()["sessions"][0]
            .get("setup_plan")
            .is_none()
    );

    assert!(matches!(
        perform_create(
            &runtime,
            &FakeSessionGit::ok(),
            &operation,
            &json!({"name":"failed-setup"}),
        ),
        Err(SessionRuntimeError::DurableFailure(_))
    ));
    assert_eq!(calls.lock().unwrap().len(), 3);
}

#[test]
fn restart_marks_an_interrupted_setup_as_an_initialize_failure() {
    let temporary = tempfile::tempdir().unwrap();
    let repository = temporary.path().join("repository");
    std::fs::create_dir_all(repository.join(".usagi")).unwrap();
    std::fs::write(
        repository.join(".usagi/config.toml"),
        "[session]\nsetup_commands = [\"non-idempotent\"]\n",
    )
    .unwrap();
    let daemon = temporary.path().join("daemon");
    let mut runtime = SessionRuntime::open(
        repository.clone(),
        &daemon,
        DaemonGeneration::new(),
        FakeSessionGit::ok(),
        SetupSessionWorktreeIo {
            calls: Arc::new(Mutex::new(Vec::new())),
            fail_on: None,
            runtime: Arc::new(Mutex::new(None)),
            observed_unlocked: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        },
    )
    .unwrap();
    let in_flight = pending_create(
        runtime
            .begin_create(
                CreateOrigin::Direct,
                &operation(),
                &json!({"name":"interrupted"}),
            )
            .unwrap(),
    )
    .unwrap();
    assert!(matches!(
        runtime.finish_create(*in_flight, Ok(())).unwrap(),
        SessionCreateCompletion::Initializing(_)
    ));
    drop(runtime);

    let reopened = SessionRuntime::open(
        repository,
        &daemon,
        DaemonGeneration::new(),
        FakeSessionGit::ok(),
        SetupSessionWorktreeIo {
            calls: Arc::new(Mutex::new(Vec::new())),
            fail_on: None,
            runtime: Arc::new(Mutex::new(None)),
            observed_unlocked: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        },
    )
    .unwrap();
    let state = reopened.state().unwrap();
    assert_eq!(state.sessions[0].lifecycle, SessionLifecycle::Failed);
    assert_eq!(
        state.sessions[0].failure.as_ref().unwrap().stage,
        FailureStage::Initialize
    );
}

/// A delegated create journals its origin, which is the only durable trace
/// that a session belongs to a composite operation whose dispatch may never
/// have happened. A plain `session_create` is complete on its own and is never
/// a recovery candidate — with or without a role (#611).
#[test]
fn only_delegated_creates_are_reported_for_recovery() {
    let (tmp, rt) = runtime(FakeSessionGit::ok());
    std::fs::write(
        tmp.path().join(".usagi/roles.toml"),
        r#"version = 1
[roles.coder]
summary = "Implement"
scopes = ["session"]
instructions = "code"
"#,
    )
    .unwrap();
    let runtime = Arc::new(Mutex::new(rt));
    let delegate = |operation: &str, payload| {
        perform_delegated_create(&runtime, &FakeSessionGit::ok(), operation, &payload).unwrap();
    };
    let create = |payload| {
        perform_create(&runtime, &FakeSessionGit::ok(), &operation(), &payload).unwrap();
    };

    let delegated = operation();
    delegate(&delegated, json!({"name":"triage"}));
    create(json!({"name":"plain"}));
    // A role-bearing create journals its role in the same key, so the recovery
    // pass must recognise the action and name as a prefix rather than by
    // whole-key equality — in both directions.
    let with_role = operation();
    delegate(&with_role, json!({"name":"triage-coder", "role":"coder"}));
    create(json!({"name":"plain-coder", "role":"coder"}));

    let candidates = runtime.lock().unwrap().delegated_sessions().unwrap();
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.name.as_str())
            .collect::<Vec<_>>(),
        ["triage", "triage-coder"]
    );
    assert_eq!(candidates[0].operation_id.to_string(), delegated);
    assert_eq!(candidates[1].operation_id.to_string(), with_role);

    // A retry under the same operation replays the create instead of making a
    // second session.
    delegate(&delegated, json!({"name":"triage"}));
    assert_eq!(
        runtime.lock().unwrap().snapshot().unwrap()["sessions"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
}

/// A session the journal does not explain is not a delegation.
///
/// Legacy adoption produces exactly that: an available session with no create
/// operation at all. Recovery must leave it alone rather than read the absence
/// of a journal entry as "nothing dispatched" and roll it back (#611).
#[test]
fn a_session_without_an_owning_operation_is_not_a_delegation_candidate() {
    let (_tmp, rt) = runtime(FakeSessionGit::ok());
    let mut state = rt.state().unwrap();
    let revision = state.state_revision;
    state.state_revision += 1;
    state.sessions.push(ManagedSession::adopt_available(
        "adopted".into(),
        Utc::now(),
    ));
    rt.store.replace_if_revision(revision, &state).unwrap();

    assert_eq!(
        rt.state().unwrap().sessions[0].lifecycle,
        SessionLifecycle::Available
    );
    assert!(rt.delegated_sessions().unwrap().is_empty());
}

/// Compensating a delegated create undoes the branch too, so the same session
/// name can be delegated again (#611).
#[test]
fn compensating_a_delegated_create_undoes_the_branch_and_frees_the_name() {
    let (tmp, rt) = runtime(FakeSessionGit::ok());
    let runtime = Arc::new(Mutex::new(rt));
    perform_delegated_create(
        &runtime,
        &FakeSessionGit::ok(),
        &operation(),
        &json!({"name":"triage"}),
    )
    .unwrap();

    let session_root = tmp.path().join(STATE_DIR).join(SESSIONS_DIR).join("triage");
    std::fs::create_dir_all(&session_root).unwrap();
    std::fs::write(session_root.join(".git"), "gitdir: /fixture").unwrap();
    let signal = TeardownSignal::new();
    perform_compensating_remove(&runtime, &signal, &operation(), "triage").unwrap();

    // The durable plan says: force, and take the branch with it.
    let pending = runtime.lock().unwrap().pending_teardowns().unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].force);
    assert!(pending[0].delete_branch);
    assert!(pending[0].force_delete_branch);

    let git = RecordingGit::new();
    let calls = Arc::clone(&git.calls);
    let reports = drain_pending_teardowns(
        &SharedSessionTeardown::new(Arc::clone(&runtime)),
        &WorktreeTeardown::new(git, SystemSessionWorktreeIo),
        &|| false,
    );
    assert_eq!(reports[0].effect_error, None);
    assert!(!session_root.exists());
    let calls = calls.lock().unwrap().clone();
    assert_eq!(
        calls.last().unwrap().1,
        vec!["branch", "-D", "--", "usagi/triage"]
    );
    // The branch is deleted from the repository root, never from the tree that
    // was just removed.
    assert_eq!(calls.last().unwrap().0, tmp.path());
    // The compensated session is gone, so it is no longer a recovery candidate.
    let candidates = || runtime.lock().unwrap().delegated_sessions().unwrap().len();
    assert_eq!(candidates(), 0);

    // The name is free again, and a plain create that reuses it belongs to the
    // user: the stale delegated journal entry must not make it a candidate.
    perform_create(
        &runtime,
        &FakeSessionGit::ok(),
        &operation(),
        &json!({"name":"triage"}),
    )
    .unwrap();
    assert_eq!(candidates(), 0);
}

/// Removing an available session safely deletes a fully merged branch.
#[test]
fn removing_an_available_session_safely_deletes_its_branch() {
    let (tmp, rt) = runtime(FakeSessionGit::ok());
    let runtime = Arc::new(Mutex::new(rt));
    perform_create(
        &runtime,
        &FakeSessionGit::ok(),
        &operation(),
        &json!({"name":"one"}),
    )
    .unwrap();
    let session_root = tmp.path().join(STATE_DIR).join(SESSIONS_DIR).join("one");
    std::fs::create_dir_all(&session_root).unwrap();
    std::fs::write(session_root.join(".git"), "gitdir: /fixture").unwrap();
    let signal = TeardownSignal::new();
    perform_remove(&runtime, &signal, &operation(), &json!({"name":"one"})).unwrap();
    let pending = runtime.lock().unwrap().pending_teardowns().unwrap();
    assert!(pending[0].delete_branch);
    assert!(!pending[0].force_delete_branch);

    let git = RecordingGit::new();
    let calls = Arc::clone(&git.calls);
    drain_pending_teardowns(
        &SharedSessionTeardown::new(Arc::clone(&runtime)),
        &WorktreeTeardown::new(git, SystemSessionWorktreeIo),
        &|| false,
    );
    assert!(calls.lock().unwrap().iter().any(|(repo, args)| {
        repo == tmp.path() && args == &["branch", "-d", "--", "usagi/one"]
    }));
}

#[test]
#[allow(clippy::too_many_lines)] // One recovery scenario proves safe failure, confirmed retry, and final cleanup.
fn removing_a_session_with_unmerged_commits_keeps_the_branch_and_failed_name() {
    struct UnmergedBranchGit {
        calls: Arc<Mutex<Vec<Vec<String>>>>,
    }
    impl GitRunner for UnmergedBranchGit {
        fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|arg| (*arg).to_owned()).collect());
            Ok(if args.get(..2) == Some(["branch", "-d"].as_slice()) {
                GitOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: "error: the branch 'usagi/one' is not fully merged".into(),
                }
            } else {
                GitOutput {
                    success: true,
                    stdout: String::new(),
                    stderr: String::new(),
                }
            })
        }
    }

    let (tmp, rt) = runtime(FakeSessionGit::ok());
    let runtime = Arc::new(Mutex::new(rt));
    perform_create(
        &runtime,
        &FakeSessionGit::ok(),
        &operation(),
        &json!({"name":"one"}),
    )
    .unwrap();
    let session_root = tmp.path().join(STATE_DIR).join(SESSIONS_DIR).join("one");
    std::fs::create_dir_all(&session_root).unwrap();
    std::fs::write(session_root.join(".git"), "gitdir: /fixture").unwrap();
    let signal = TeardownSignal::new();
    perform_remove(&runtime, &signal, &operation(), &json!({"name":"one"})).unwrap();

    let calls = Arc::new(Mutex::new(Vec::new()));
    let reports = drain_pending_teardowns(
        &SharedSessionTeardown::new(Arc::clone(&runtime)),
        &WorktreeTeardown::new(
            UnmergedBranchGit {
                calls: Arc::clone(&calls),
            },
            SystemSessionWorktreeIo,
        ),
        &|| false,
    );

    assert!(
        reports[0]
            .effect_error
            .as_deref()
            .is_some_and(|error| error.contains("not fully merged"))
    );
    let state = runtime.lock().unwrap().state().unwrap();
    assert_eq!(state.sessions[0].name, "one");
    assert_eq!(state.sessions[0].lifecycle, SessionLifecycle::Failed);
    assert!(
        state.sessions[0]
            .failure
            .as_ref()
            .is_some_and(|failure| failure.summary.contains("not fully merged"))
    );
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .any(|args| { args == &["branch", "-d", "--", "usagi/one"] })
    );
    drop(state);

    perform_remove(
        &runtime,
        &signal,
        &operation(),
        &json!({
            "name":"one",
            "force":true,
            "force_delete_branch":true,
        }),
    )
    .unwrap();
    let reports = drain_pending_teardowns(
        &SharedSessionTeardown::new(Arc::clone(&runtime)),
        &WorktreeTeardown::new(
            UnmergedBranchGit {
                calls: Arc::clone(&calls),
            },
            SystemSessionWorktreeIo,
        ),
        &|| false,
    );

    assert_eq!(reports[0].effect_error, None);
    assert!(runtime.lock().unwrap().state().unwrap().sessions.is_empty());
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .any(|args| { args == &["branch", "-D", "--", "usagi/one"] })
    );
}

#[test]
fn forced_branch_delete_requires_worktree_force() {
    let (_tmp, rt) = runtime(FakeSessionGit::ok());
    let runtime = Arc::new(Mutex::new(rt));
    perform_create(
        &runtime,
        &FakeSessionGit::ok(),
        &operation(),
        &json!({"name":"one"}),
    )
    .unwrap();

    let error = perform_remove(
        &runtime,
        &TeardownSignal::new(),
        &operation(),
        &json!({"name":"one", "force_delete_branch":true}),
    )
    .unwrap_err();

    assert_eq!(error, SessionRuntimeError::InvalidRequest);
    assert_eq!(
        runtime.lock().unwrap().state().unwrap().sessions[0].lifecycle,
        SessionLifecycle::Available
    );
}

#[test]
fn removing_a_failed_session_deletes_its_branch() {
    let (tmp, rt) = runtime(FakeSessionGit::ok());
    let runtime = Arc::new(Mutex::new(rt));
    perform_create(
        &runtime,
        &FakeSessionGit::ok(),
        &operation(),
        &json!({"name":"one"}),
    )
    .unwrap();
    {
        let runtime = runtime.lock().unwrap();
        let mut state = runtime.state().unwrap();
        let revision = state.state_revision;
        state.sessions[0].lifecycle = SessionLifecycle::Failed;
        state.sessions[0].failure = Some(Failure {
            stage: FailureStage::Create,
            summary: "create failed".into(),
        });
        runtime.store.replace_if_revision(revision, &state).unwrap();
    }
    let session_root = tmp.path().join(STATE_DIR).join(SESSIONS_DIR).join("one");
    std::fs::create_dir_all(&session_root).unwrap();
    std::fs::write(session_root.join(".git"), "gitdir: /fixture").unwrap();

    let signal = TeardownSignal::new();
    perform_remove(&runtime, &signal, &operation(), &json!({"name":"one"})).unwrap();
    let pending = runtime.lock().unwrap().pending_teardowns().unwrap();
    assert!(pending[0].delete_branch);
    assert!(!pending[0].force_delete_branch);

    let git = RecordingGit::new();
    let calls = Arc::clone(&git.calls);
    let reports = drain_pending_teardowns(
        &SharedSessionTeardown::new(Arc::clone(&runtime)),
        &WorktreeTeardown::new(git, SystemSessionWorktreeIo),
        &|| false,
    );

    assert_eq!(reports[0].effect_error, None);
    assert!(!session_root.exists());
    assert!(runtime.lock().unwrap().state().unwrap().sessions.is_empty());
    assert!(calls.lock().unwrap().iter().any(|(repo, args)| {
        repo == tmp.path() && args == &["branch", "-d", "--", "usagi/one"]
    }));
}

/// The branch deletion is part of the compensation, so its failure is a
/// teardown failure: the record stays diagnosable rather than being reported
/// as a clean rollback.
#[test]
fn a_failed_branch_deletion_fails_the_compensating_teardown() {
    /// Succeeds at removing the worktree and refuses to delete the branch.
    struct BranchLockedGit;
    impl GitRunner for BranchLockedGit {
        fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
            Ok(GitOutput {
                success: args[0] != "branch",
                stdout: String::new(),
                stderr: "error: cannot delete branch 'usagi/one' used by worktree".into(),
            })
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let session_root = tmp.path().join(STATE_DIR).join(SESSIONS_DIR).join("one");
    std::fs::create_dir_all(&session_root).unwrap();
    let data_home = tmp.path().join("daemon");
    std::fs::create_dir_all(&data_home).unwrap();
    let error = WorktreeTeardown::new(BranchLockedGit, SystemSessionWorktreeIo)
        .tear_down(&PendingTeardown {
            delete_branch: true,
            repository_root: tmp.path().to_path_buf(),
            data_home,
            session_container: tmp.path().join(STATE_DIR).join(SESSIONS_DIR),
            session_root,
            ..confined_teardown()
        })
        .unwrap_err();
    assert!(error.contains("git branch delete failed"), "{error}");
}

#[test]
fn perform_remove_accepts_without_touching_the_worktree_and_hands_it_to_the_worker() {
    let (tmp, rt) = runtime(FakeSessionGit::ok());
    let runtime = Arc::new(Mutex::new(rt));
    perform_create(
        &runtime,
        &FakeSessionGit::ok(),
        &operation(),
        &json!({"name":"one"}),
    )
    .unwrap();
    // Materialize a linked worktree so a teardown would have to invoke Git.
    let session_root = tmp.path().join(STATE_DIR).join(SESSIONS_DIR).join("one");
    std::fs::create_dir_all(&session_root).unwrap();
    std::fs::write(session_root.join(".git"), "gitdir: /fixture").unwrap();
    let signal = TeardownSignal::new();

    let reply = perform_remove(&runtime, &signal, &operation(), &json!({"name":"one"})).unwrap();

    // The reply is the acceptance: the row is `deleting`, the tree is still
    // there, and the worker was woken.
    assert_eq!(reply.body["sessions"][0]["name"], "one");
    assert_eq!(reply.body["sessions"][0]["lifecycle"], "deleting");
    assert!(session_root.exists());
    assert!(signal.wait(std::time::Duration::from_millis(1)));

    // A row in a transient state is work only this daemon can finish, so the
    // workspace it belongs to must not be given back while it is there.
    assert!(runtime.lock().unwrap().has_unfinished_work().unwrap());

    // The pending teardown is derived from that durable state alone.
    let pending = runtime.lock().unwrap().pending_teardowns().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].name, "one");
    assert_eq!(pending[0].session_root, session_root);

    // Draining it removes the tree and retires the record.
    let calls = Arc::new(AtomicUsize::new(0));
    let reports = drain_pending_teardowns(
        &SharedSessionTeardown::new(Arc::clone(&runtime)),
        &WorktreeTeardown::new(
            CountingGit {
                calls: Arc::clone(&calls),
            },
            SystemSessionWorktreeIo,
        ),
        &|| false,
    );
    assert_eq!(reports[0].effect_error, None);
    assert_eq!(reports[0].finalize_error, None);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(!session_root.exists());
    assert!(
        runtime.lock().unwrap().snapshot().unwrap()["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        runtime
            .lock()
            .unwrap()
            .pending_teardowns()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_second_remove_of_a_deleting_session_returns_the_operation_already_in_flight() {
    let (_tmp, rt) = runtime(FakeSessionGit::ok());
    let runtime = Arc::new(Mutex::new(rt));
    perform_create(
        &runtime,
        &FakeSessionGit::ok(),
        &operation(),
        &json!({"name":"one"}),
    )
    .unwrap();
    let signal = TeardownSignal::new();
    let accepted = perform_remove(&runtime, &signal, &operation(), &json!({"name":"one"})).unwrap();

    // A retry with a fresh operation ID must not admit a second teardown.
    let again = perform_remove(&runtime, &signal, &operation(), &json!({"name":"one"})).unwrap();

    assert_eq!(again.operation_id, accepted.operation_id);
    assert_eq!(again.revision, accepted.revision);
    assert_eq!(
        runtime.lock().unwrap().pending_teardowns().unwrap().len(),
        1
    );
    assert_eq!(runtime.lock().unwrap().state().unwrap().operations.len(), 2);
}

/// Losing the accepted response does not widen the operation identity. The
/// exact force intent replays while the opposite intent conflicts, in both
/// directions, and the one admitted plan remains the only worktree effect.
#[test]
#[allow(clippy::too_many_lines)] // One scenario crosses accepted/restart/succeeded boundaries.
fn remove_force_is_part_of_the_durable_identity_before_and_after_restart() {
    for (first_force, conflicting_force) in [(false, true), (true, false)] {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let state_dir = tmp.path().join("daemon");
        let runtime = Arc::new(Mutex::new(
            SessionRuntime::open(
                tmp.path().to_path_buf(),
                &state_dir,
                DaemonGeneration::new(),
                FakeSessionGit::ok(),
                SystemSessionWorktreeIo,
            )
            .unwrap(),
        ));
        perform_create(
            &runtime,
            &FakeSessionGit::ok(),
            &operation(),
            &json!({"name":"one"}),
        )
        .unwrap();
        let session_root = tmp.path().join(STATE_DIR).join(SESSIONS_DIR).join("one");
        std::fs::create_dir_all(&session_root).unwrap();
        std::fs::write(session_root.join(".git"), "gitdir: /fixture").unwrap();
        let operation = operation();
        let signal = TeardownSignal::new();
        let request = json!({"name":"one", "force":first_force});

        // Model response loss by discarding the first accepted reply.
        perform_remove(&runtime, &signal, &operation, &request).unwrap();
        let replayed = perform_remove(&runtime, &signal, &operation, &request).unwrap();
        assert_eq!(replayed.operation_id, operation);
        assert_eq!(replayed.body["sessions"][0]["lifecycle"], "deleting");
        assert_eq!(
            perform_remove(
                &runtime,
                &signal,
                &operation,
                &json!({"name":"one", "force":conflicting_force}),
            ),
            Err(SessionRuntimeError::IdempotencyConflict)
        );
        assert_eq!(
            runtime.lock().unwrap().pending_teardowns().unwrap().len(),
            1
        );

        // An accepted operation remains replayable after daemon restart;
        // the durable plan is still the only queued effect.
        drop(runtime);
        let runtime = Arc::new(Mutex::new(
            SessionRuntime::open(
                tmp.path().to_path_buf(),
                &state_dir,
                DaemonGeneration::new(),
                FakeSessionGit::ok(),
                SystemSessionWorktreeIo,
            )
            .unwrap(),
        ));
        let after_accepted_restart =
            perform_remove(&runtime, &signal, &operation, &request).unwrap();
        assert_eq!(after_accepted_restart.operation_id, operation);
        assert_eq!(
            runtime.lock().unwrap().pending_teardowns().unwrap().len(),
            1
        );
        assert_eq!(
            perform_remove(
                &runtime,
                &signal,
                &operation,
                &json!({"name":"one", "force":conflicting_force}),
            ),
            Err(SessionRuntimeError::IdempotencyConflict)
        );

        let calls = Arc::new(AtomicUsize::new(0));
        drain_pending_teardowns(
            &SharedSessionTeardown::new(Arc::clone(&runtime)),
            &WorktreeTeardown::new(
                CountingGit {
                    calls: Arc::clone(&calls),
                },
                SystemSessionWorktreeIo,
            ),
            &|| false,
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let succeeded = perform_remove(&runtime, &signal, &operation, &request).unwrap();
        assert_eq!(succeeded.operation_id, operation);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        drop(runtime);

        // A terminal successful outcome also survives restart without a
        // replacement worktree effect.
        let restarted = Arc::new(Mutex::new(
            SessionRuntime::open(
                tmp.path().to_path_buf(),
                &state_dir,
                DaemonGeneration::new(),
                CountingGit {
                    calls: Arc::clone(&calls),
                },
                SystemSessionWorktreeIo,
            )
            .unwrap(),
        ));
        let after_restart = perform_remove(&restarted, &signal, &operation, &request).unwrap();
        assert_eq!(after_restart.operation_id, operation);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            perform_remove(
                &restarted,
                &signal,
                &operation,
                &json!({"name":"one", "force":conflicting_force}),
            ),
            Err(SessionRuntimeError::IdempotencyConflict)
        );
    }
}

#[test]
fn legacy_remove_keys_replay_only_while_the_delete_plan_proves_the_intent() {
    let (tmp, rt) = runtime(FakeSessionGit::ok());
    let runtime = Arc::new(Mutex::new(rt));
    perform_create(
        &runtime,
        &FakeSessionGit::ok(),
        &operation(),
        &json!({"name":"one"}),
    )
    .unwrap();
    let operation = operation();
    let signal = TeardownSignal::new();
    let request = json!({"name":"one", "force":true});
    perform_remove(&runtime, &signal, &operation, &request).unwrap();

    // Simulate a snapshot written before remove keys carried force/origin.
    {
        let runtime = runtime.lock().unwrap();
        let mut legacy = runtime.state().unwrap();
        let revision = legacy.state_revision;
        legacy.operations.last_mut().unwrap().semantic_key =
            semantic_key(SessionAction::Remove, "one");
        runtime
            .store
            .replace_if_revision(revision, &legacy)
            .unwrap();
    }
    assert!(perform_remove(&runtime, &signal, &operation, &request).is_ok());
    assert_eq!(
        perform_remove(
            &runtime,
            &signal,
            &operation,
            &json!({"name":"one", "force":false}),
        ),
        Err(SessionRuntimeError::IdempotencyConflict)
    );

    // The retained plan proves the same intent across restart too.
    let state_dir = tmp.path().join("daemon");
    drop(runtime);
    let restarted = Arc::new(Mutex::new(
        SessionRuntime::open(
            tmp.path().to_path_buf(),
            &state_dir,
            DaemonGeneration::new(),
            FakeSessionGit::ok(),
            SystemSessionWorktreeIo,
        )
        .unwrap(),
    ));
    assert!(perform_remove(&restarted, &signal, &operation, &request).is_ok());

    drain_pending_teardowns(
        &SharedSessionTeardown::new(Arc::clone(&restarted)),
        &WorktreeTeardown::new(FakeSessionGit::ok(), SystemSessionWorktreeIo),
        &|| false,
    );
    // Success retires the session and its plan. The legacy key can no longer
    // prove either force value, so both guesses fail closed.
    for force in [false, true] {
        assert_eq!(
            perform_remove(
                &restarted,
                &signal,
                &operation,
                &json!({"name":"one", "force":force}),
            ),
            Err(SessionRuntimeError::IdempotencyConflict)
        );
    }
}

#[test]
fn compensating_and_requested_removes_are_distinct_durable_intents() {
    let (_tmp, rt) = runtime(FakeSessionGit::ok());
    let runtime = Arc::new(Mutex::new(rt));
    perform_delegated_create(
        &runtime,
        &FakeSessionGit::ok(),
        &operation(),
        &json!({"name":"triage"}),
    )
    .unwrap();
    let operation = operation();
    let signal = TeardownSignal::new();
    perform_compensating_remove(&runtime, &signal, &operation, "triage").unwrap();

    assert_eq!(
        perform_remove(
            &runtime,
            &signal,
            &operation,
            &json!({"name":"triage", "force":true}),
        ),
        Err(SessionRuntimeError::IdempotencyConflict)
    );
    let state = runtime.lock().unwrap().state().unwrap();
    assert_eq!(state.operations.len(), 2);
    assert_eq!(
        state.operations[1].semantic_key,
        "remove:triage:origin=compensating:force=true:force_delete_branch=true"
    );
    assert!(
        state.sessions[0]
            .delete_plan
            .as_ref()
            .unwrap()
            .delete_branch
    );
}

#[test]
fn legacy_compensation_replay_requires_a_matching_session_and_forced_branch_delete_flags() {
    let (_tmp, rt) = runtime(FakeSessionGit::ok());
    let runtime = Arc::new(Mutex::new(rt));
    perform_delegated_create(
        &runtime,
        &FakeSessionGit::ok(),
        &operation(),
        &json!({"name":"triage"}),
    )
    .unwrap();
    let operation_id = operation();
    let signal = TeardownSignal::new();
    perform_compensating_remove(&runtime, &signal, &operation_id, "triage").unwrap();

    let mut state = runtime.lock().unwrap().state().unwrap();
    let mut legacy_operation = state.operations.last().unwrap().clone();
    legacy_operation.semantic_key = semantic_key(SessionAction::Remove, "triage");
    let requested_key = remove_semantic_key(RemoveKind::Compensating, "triage", true, true);
    assert!(remove_operation_matches(
        &state,
        &legacy_operation,
        RemoveKind::Compensating,
        "triage",
        true,
        true,
        &requested_key,
    ));

    let mut wrong_name_operation = legacy_operation.clone();
    wrong_name_operation.semantic_key = semantic_key(SessionAction::Remove, "missing");
    assert!(!remove_operation_matches(
        &state,
        &wrong_name_operation,
        RemoveKind::Compensating,
        "missing",
        true,
        true,
        &remove_semantic_key(RemoveKind::Compensating, "missing", true, true),
    ));
    let mut wrong_id_operation = legacy_operation.clone();
    wrong_id_operation.operation_id = OperationId::new();
    assert!(!remove_operation_matches(
        &state,
        &wrong_id_operation,
        RemoveKind::Compensating,
        "triage",
        true,
        true,
        &requested_key,
    ));

    let saved_plan = state.sessions[0].delete_plan.take();
    assert!(!remove_operation_matches(
        &state,
        &legacy_operation,
        RemoveKind::Compensating,
        "triage",
        true,
        true,
        &requested_key,
    ));
    state.sessions[0].delete_plan = saved_plan;

    let plan = state.sessions[0].delete_plan.as_mut().unwrap();
    plan.delete_branch = false;
    assert!(!remove_operation_matches(
        &state,
        &legacy_operation,
        RemoveKind::Compensating,
        "triage",
        true,
        true,
        &requested_key,
    ));
    let plan = state.sessions[0].delete_plan.as_mut().unwrap();
    plan.delete_branch = true;
    plan.force_delete_branch = false;
    assert!(!remove_operation_matches(
        &state,
        &legacy_operation,
        RemoveKind::Compensating,
        "triage",
        true,
        true,
        &requested_key,
    ));
}

#[test]
fn a_teardown_failure_records_the_reason_on_a_failed_row_and_frees_the_name_after_removal() {
    let (_tmp, rt) = runtime(FakeSessionGit::ok());
    let runtime = Arc::new(Mutex::new(rt));
    perform_create(
        &runtime,
        &FakeSessionGit::ok(),
        &operation(),
        &json!({"name":"one"}),
    )
    .unwrap();
    let signal = TeardownSignal::new();
    perform_remove(&runtime, &signal, &operation(), &json!({"name":"one"})).unwrap();

    let reports = drain_pending_teardowns(
        &SharedSessionTeardown::new(Arc::clone(&runtime)),
        &FailingTeardown,
        &|| false,
    );

    assert!(reports[0].effect_error.is_some());
    assert_eq!(reports[0].finalize_error, None);
    let listed = runtime.lock().unwrap().snapshot().unwrap();
    assert_eq!(listed["sessions"][0]["lifecycle"], "failed");
    let summary = listed["sessions"][0]["failure"]["summary"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(summary.contains("could not remove the session worktree \"one\""));
    assert!(summary.contains("contains modified or untracked files"));
    // The failed row still owns the name; removing it frees it again.
    assert!(
        runtime
            .lock()
            .unwrap()
            .pending_teardowns()
            .unwrap()
            .is_empty()
    );
    perform_remove(&runtime, &signal, &operation(), &json!({"name":"one"})).unwrap();
    drain_pending_teardowns(
        &SharedSessionTeardown::new(Arc::clone(&runtime)),
        &WorktreeTeardown::new(FakeSessionGit::ok(), SystemSessionWorktreeIo),
        &|| false,
    );
    assert!(
        perform_create(
            &runtime,
            &FakeSessionGit::ok(),
            &operation(),
            &json!({"name":"one"})
        )
        .is_ok()
    );
}

#[test]
fn an_interrupted_teardown_is_resumed_after_restart_instead_of_failing() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    let state_dir = tmp.path().join("daemon");
    let session_root = tmp.path().join(STATE_DIR).join(SESSIONS_DIR).join("one");
    let first = Arc::new(Mutex::new(
        SessionRuntime::open(
            tmp.path().to_path_buf(),
            &state_dir,
            DaemonGeneration::new(),
            FakeSessionGit::ok(),
            SystemSessionWorktreeIo,
        )
        .unwrap(),
    ));
    perform_create(
        &first,
        &FakeSessionGit::ok(),
        &operation(),
        &json!({"name":"one"}),
    )
    .unwrap();
    std::fs::create_dir_all(&session_root).unwrap();
    std::fs::write(session_root.join("file"), "work").unwrap();
    let signal = TeardownSignal::new();
    perform_remove(&first, &signal, &operation(), &json!({"name":"one"})).unwrap();
    // The daemon dies here: the record stays `Deleting` with its durable
    // delete plan, and the worktree is still on disk.
    drop(first);

    let restarted = Arc::new(Mutex::new(
        SessionRuntime::open(
            tmp.path().to_path_buf(),
            &state_dir,
            DaemonGeneration::new(),
            FakeSessionGit::ok(),
            SystemSessionWorktreeIo,
        )
        .unwrap(),
    ));

    // Restart does not fail the interrupted delete: it is pending again.
    let listed = restarted.lock().unwrap().snapshot().unwrap();
    assert_eq!(listed["sessions"][0]["lifecycle"], "deleting");
    let pending = restarted.lock().unwrap().pending_teardowns().unwrap();
    assert_eq!(pending.len(), 1);

    // The new daemon's worker completes the operation the previous
    // generation journaled.
    drain_pending_teardowns(
        &SharedSessionTeardown::new(Arc::clone(&restarted)),
        &WorktreeTeardown::new(FakeSessionGit::ok(), SystemSessionWorktreeIo),
        &|| false,
    );
    assert!(!session_root.exists());
    assert!(
        restarted.lock().unwrap().snapshot().unwrap()["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn restart_rejects_persisted_path_names_without_touching_a_sentinel() {
    for name in [
        "/tmp/victim",
        "../victim",
        "nested/victim",
        "nested\\victim",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let state_dir = tmp.path().join("daemon");
        let mut runtime = SessionRuntime::open(
            tmp.path().to_path_buf(),
            &state_dir,
            DaemonGeneration::new(),
            FakeSessionGit::ok(),
            FakeSessionWorktreeIo {
                occupied: false,
                build_calls: Arc::new(AtomicUsize::new(0)),
            },
        )
        .unwrap();
        runtime
            .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
            .unwrap();
        let state_path = state_dir.join("sessions.json");
        let mut document: Value =
            serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
        document["state"]["sessions"][0]["name"] = Value::String(name.into());
        std::fs::write(&state_path, serde_json::to_vec(&document).unwrap()).unwrap();
        let sentinel = tmp.path().join("victim/sentinel");
        std::fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
        std::fs::write(&sentinel, "keep").unwrap();
        drop(runtime);

        let git_calls = Arc::new(AtomicUsize::new(0));
        let reopened = SessionRuntime::open(
            tmp.path().to_path_buf(),
            &state_dir,
            DaemonGeneration::new(),
            CountingGit {
                calls: Arc::clone(&git_calls),
            },
            SystemSessionWorktreeIo,
        );

        assert!(matches!(reopened, Err(SessionRuntimeError::Storage)));
        assert!(sentinel.exists(), "malicious persisted name was {name}");
        assert_eq!(git_calls.load(Ordering::SeqCst), 0);
    }
}

#[cfg(unix)]
#[test]
fn real_teardown_rejects_a_symlinked_session_ancestor_with_zero_effect() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().unwrap();
    let repository = tmp.path().join("repository");
    let data_home = tmp.path().join("data");
    let victim_session = tmp.path().join("victim/one");
    std::fs::create_dir_all(repository.join(STATE_DIR)).unwrap();
    std::fs::create_dir_all(&data_home).unwrap();
    std::fs::create_dir_all(&victim_session).unwrap();
    let sentinel = victim_session.join("sentinel");
    std::fs::write(&sentinel, "keep").unwrap();
    let container = repository.join(STATE_DIR).join(SESSIONS_DIR);
    symlink(tmp.path().join("victim"), &container).unwrap();
    let git_calls = Arc::new(AtomicUsize::new(0));
    let teardown = PendingTeardown {
        session_id: SessionId::new(),
        operation_id: OperationId::new(),
        name: "one".into(),
        repository_root: repository,
        data_home,
        session_container: container.clone(),
        session_root: container.join("one"),
        force: true,
        delete_branch: false,
        branch_name: None,
        force_delete_branch: false,
        merged_head_oid: None,
    };

    let result = WorktreeTeardown::new(
        CountingGit {
            calls: Arc::clone(&git_calls),
        },
        SystemSessionWorktreeIo,
    )
    .tear_down(&teardown);

    assert!(result.unwrap_err().contains("symlinked session ancestor"));
    assert!(sentinel.exists());
    assert_eq!(git_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn protected_roots_are_never_valid_teardown_targets() {
    let repository = Path::new("/repository");
    let data_home = Path::new("/data");
    assert!(protected_teardown_target(repository, repository, data_home));
    assert!(protected_teardown_target(data_home, repository, data_home));
    assert!(protected_teardown_target(
        Path::new("/"),
        repository,
        data_home
    ));
    assert!(!protected_teardown_target(
        Path::new("/repository/.usagi/sessions/one"),
        repository,
        data_home,
    ));
}

#[test]
fn teardown_confinement_errors_have_zero_git_and_filesystem_effects() {
    let git_calls = Arc::new(AtomicUsize::new(0));
    let remove_calls = Arc::new(AtomicUsize::new(0));

    let mut invalid_name = confined_teardown();
    invalid_name.name = "../victim".into();
    let mut mismatched_shape = confined_teardown();
    mismatched_shape.session_root = PathBuf::from("/repo");

    let cases = [
        (invalid_name, ConfinementIo::new(Arc::clone(&remove_calls))),
        (
            mismatched_shape,
            ConfinementIo::new(Arc::clone(&remove_calls)),
        ),
        {
            let mut io = ConfinementIo::new(Arc::clone(&remove_calls));
            io.canonical.insert(PathBuf::from("/repo"), None);
            (confined_teardown(), io)
        },
        {
            let mut io = ConfinementIo::new(Arc::clone(&remove_calls));
            io.canonical.insert(PathBuf::from("/data"), None);
            (confined_teardown(), io)
        },
        {
            let mut io = ConfinementIo::new(Arc::clone(&remove_calls));
            io.canonical
                .insert(PathBuf::from("/repo/.usagi/sessions"), None);
            (confined_teardown(), io)
        },
        {
            let mut io = ConfinementIo::new(Arc::clone(&remove_calls));
            io.canonical.insert(
                PathBuf::from("/repo/.usagi/sessions"),
                Some(PathBuf::from("/escape")),
            );
            (confined_teardown(), io)
        },
        {
            let mut io = ConfinementIo::new(Arc::clone(&remove_calls));
            io.canonical.insert(
                PathBuf::from("/data"),
                Some(PathBuf::from("/repo/.usagi/sessions")),
            );
            (confined_teardown(), io)
        },
        {
            let mut io = ConfinementIo::new(Arc::clone(&remove_calls));
            io.occupied = true;
            io.canonical
                .insert(PathBuf::from("/repo/.usagi/sessions/one"), None);
            (confined_teardown(), io)
        },
        {
            let mut io = ConfinementIo::new(Arc::clone(&remove_calls));
            io.occupied = true;
            io.canonical.insert(
                PathBuf::from("/repo/.usagi/sessions/one"),
                Some(PathBuf::from("/victim")),
            );
            (confined_teardown(), io)
        },
    ];

    for (teardown, io) in cases {
        assert!(
            WorktreeTeardown::new(
                CountingGit {
                    calls: Arc::clone(&git_calls),
                },
                io,
            )
            .tear_down(&teardown)
            .is_err()
        );
    }
    assert_eq!(git_calls.load(Ordering::SeqCst), 0);
    assert_eq!(remove_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn teardown_confinement_preserves_absent_target_idempotency() {
    let remove_calls = Arc::new(AtomicUsize::new(0));
    let io_contract = ConfinementIo::new(Arc::clone(&remove_calls));
    io_contract.remove_file_best_effort(Path::new("/unused"));
    assert!(!io_contract.is_repo_root(Path::new("/unused")));
    assert!(!io_contract.is_linked_worktree(Path::new("/unused")));
    io_contract
        .build_session_tree(
            &FakeSessionGit::ok(),
            Path::new("/source"),
            Path::new("/destination"),
            "branch",
            None,
        )
        .unwrap();
    WorktreeTeardown::new(FakeSessionGit::ok(), io_contract)
        .tear_down(&confined_teardown())
        .unwrap();
    assert_eq!(remove_calls.load(Ordering::SeqCst), 1);

    let mut occupied = ConfinementIo::new(Arc::clone(&remove_calls));
    occupied.occupied = true;
    WorktreeTeardown::new(FakeSessionGit::ok(), occupied)
        .tear_down(&confined_teardown())
        .unwrap();
    assert_eq!(remove_calls.load(Ordering::SeqCst), 2);
}

#[test]
fn finalizing_a_teardown_twice_reports_durable_truth_without_a_stale_write() {
    let (_tmp, rt) = runtime(FakeSessionGit::ok());
    let runtime = Arc::new(Mutex::new(rt));
    perform_create(
        &runtime,
        &FakeSessionGit::ok(),
        &operation(),
        &json!({"name":"one"}),
    )
    .unwrap();
    let signal = TeardownSignal::new();
    perform_remove(&runtime, &signal, &operation(), &json!({"name":"one"})).unwrap();
    let pending = runtime.lock().unwrap().pending_teardowns().unwrap()[0].clone();

    let completed = runtime
        .lock()
        .unwrap()
        .finish_teardown(&pending, Ok(()))
        .unwrap();
    // The record is gone, so a duplicate finalization is a no-op that
    // reports the current state rather than writing a stale outcome.
    let repeated = runtime
        .lock()
        .unwrap()
        .finish_teardown(&pending, Ok(()))
        .unwrap();

    assert_eq!(repeated.revision, completed.revision);
    assert!(repeated.body["sessions"].as_array().unwrap().is_empty());
}

#[test]
fn the_shared_teardown_journal_reports_an_unavailable_session_owner() {
    let (_tmp, rt) = runtime(FakeSessionGit::ok());
    let shared = Arc::new(Mutex::new(rt));
    perform_create(
        &shared,
        &FakeSessionGit::ok(),
        &operation(),
        &json!({"name":"one"}),
    )
    .unwrap();
    let signal = TeardownSignal::new();
    perform_remove(&shared, &signal, &operation(), &json!({"name":"one"})).unwrap();
    let journal = SharedSessionTeardown::new(Arc::clone(&shared));
    let pending = journal.pending();
    assert_eq!(pending.len(), 1);
    poison_lock(&shared);

    // A poisoned session lock leaves the record `Deleting`, so the next
    // drain retries it instead of losing the teardown.
    assert_eq!(
        journal.finish(&pending[0], Ok(())),
        Err("session lifecycle owner is unavailable".into())
    );
    assert!(journal.pending().is_empty(), "the poisoned read is empty");
}

#[test]
fn perform_create_and_remove_replay_a_completed_operation_under_the_lock() {
    let (_tmp, rt) = runtime(FakeSessionGit::ok());
    let runtime = Arc::new(Mutex::new(rt));
    let create_op = operation();
    let created = perform_create(
        &runtime,
        &FakeSessionGit::ok(),
        &create_op,
        &json!({"name":"one"}),
    )
    .unwrap();
    let replayed_create = perform_create(
        &runtime,
        &FakeSessionGit::ok(),
        &create_op,
        &json!({"name":"one"}),
    )
    .unwrap();
    assert_eq!(created.body, replayed_create.body);

    let signal = TeardownSignal::new();
    let remove_op = operation();
    perform_remove(&runtime, &signal, &remove_op, &json!({"name":"one"})).unwrap();
    drain_pending_teardowns(
        &SharedSessionTeardown::new(Arc::clone(&runtime)),
        &WorktreeTeardown::new(FakeSessionGit::ok(), SystemSessionWorktreeIo),
        &|| false,
    );
    let replayed_remove =
        perform_remove(&runtime, &signal, &remove_op, &json!({"name":"one"})).unwrap();
    assert!(replayed_remove.body.get("sessions").is_some());
}

#[test]
fn perform_create_maps_a_poisoned_session_lock_to_storage() {
    // Poisoned before begin: the first re-lock fails.
    let (_tmp, rt) = runtime(FakeSessionGit::ok());
    let shared = Arc::new(Mutex::new(rt));
    poison_lock(&shared);
    assert!(matches!(
        perform_create(
            &shared,
            &FakeSessionGit::ok(),
            &operation(),
            &json!({"name":"one"})
        ),
        Err(SessionRuntimeError::Storage)
    ));

    // Poisoned mid-build: begin succeeds, the finish re-lock fails.
    let (_tmp, rt) = runtime(FakeSessionGit::ok());
    let shared = Arc::new(Mutex::new(rt));
    let git = PoisoningGit {
        runtime: Arc::downgrade(&shared),
    };
    assert!(matches!(
        perform_create(&shared, &git, &operation(), &json!({"name":"one"})),
        Err(SessionRuntimeError::Storage)
    ));
}

#[test]
fn perform_remove_maps_a_poisoned_session_lock_to_storage() {
    // The admission is the only lock this path takes, so a poisoned session
    // lock is the one way it fails without reaching the reducer.
    let (_tmp, rt) = runtime(FakeSessionGit::ok());
    let shared = Arc::new(Mutex::new(rt));
    poison_lock(&shared);

    assert!(matches!(
        perform_remove(
            &shared,
            &TeardownSignal::new(),
            &operation(),
            &json!({"name":"one"})
        ),
        Err(SessionRuntimeError::Storage)
    ));
}

// 1 つの決定表を分けると読み手が追う状態が増えるため、この関数はまとめて置く。
#[allow(clippy::too_many_lines)]
#[test]
fn production_logic_coverage_contract() {
    let path = Path::new("/fake");
    let failing_io = FailingSessionWorktreeIo;
    failing_io.remove_file_best_effort(path);
    assert!(!failing_io.path_occupied(path));
    assert_eq!(failing_io.canonical_path(path), Some(path.into()));
    assert!(!failing_io.is_repo_root(path));
    assert!(!failing_io.is_linked_worktree(path));
    failing_io
        .build_session_tree(&FakeSessionGit::ok(), path, path, "branch", None)
        .unwrap();
    let fake_io = FakeSessionWorktreeIo {
        occupied: false,
        build_calls: Arc::new(AtomicUsize::new(0)),
    };
    assert_eq!(fake_io.canonical_path(path), Some(path.into()));
    assert!(fake_io.is_linked_worktree(path));
    fake_io
        .remove_session_tree(&FakeSessionGit::ok(), path, false)
        .unwrap();
    PoisoningGit {
        runtime: std::sync::Weak::new(),
    }
    .run(path, &[])
    .unwrap();

    let messages = [
        (
            SessionRuntimeError::InvalidRequest,
            "invalid session request",
        ),
        (
            SessionRuntimeError::InvalidOperation,
            "invalid operation identity",
        ),
        (
            SessionRuntimeError::DuplicateOperation,
            "operation identity conflicts with an existing request",
        ),
        (
            SessionRuntimeError::IdempotencyConflict,
            "operation id was reused with a different request",
        ),
        (
            SessionRuntimeError::AgentFailure {
                code: ErrorCode::Internal,
                message: "agent".into(),
            },
            "agent",
        ),
        (
            SessionRuntimeError::ScopeUnavailable,
            "session scope is not available",
        ),
        (
            SessionRuntimeError::PermissionDenied,
            "caller did not create the target session",
        ),
        (SessionRuntimeError::UnknownSession, "session was not found"),
        (
            SessionRuntimeError::Rejected,
            "could not create the session worktree; see the daemon log for details",
        ),
        (
            SessionRuntimeError::Storage,
            "daemon could not persist session lifecycle state",
        ),
        // A delegation failure reports the dispatch refusal it wraps; the
        // reconcile state travels in `details`, not in the message.
        (
            SessionRuntimeError::Delegation(DelegationFailure {
                code: ErrorCode::Unavailable,
                message: "dispatch runtime executable is unavailable".into(),
                session_id: SessionId::new(),
                run_operation_id: OperationId::new().to_string(),
                reconcile: DelegationReconcile::Compensated,
            }),
            "dispatch runtime executable is unavailable",
        ),
    ];
    for (error, expected) in messages {
        assert_eq!(error.safe_message(), expected);
    }
    assert_eq!(
        worktree_failure_detail("\u{1}"),
        "Git rejected workspace creation"
    );
    assert_eq!(
        session_name(&json!({"name": ""})),
        Err(SessionRuntimeError::InvalidRequest)
    );
    assert_eq!(session_name(&json!({"label": "alias"})), Ok("alias".into()));
    assert_eq!(
        WorktreeTeardown::new(FakeSessionGit::ok(), FailingSessionWorktreeIo).tear_down(
            &PendingTeardown {
                session_id: SessionId::new(),
                operation_id: OperationId::new(),
                name: "one".into(),
                repository_root: PathBuf::from("/repo"),
                data_home: PathBuf::from("/data"),
                session_container: PathBuf::from("/repo/.usagi/sessions"),
                session_root: PathBuf::from("/repo/.usagi/sessions/one"),
                force: false,
                delete_branch: false,
                branch_name: None,
                force_delete_branch: false,
                merged_head_oid: None,
            }
        ),
        Err("injected remove failure".into())
    );

    let (tmp, mut runtime) = runtime(FakeSessionGit::ok());
    let state = runtime.state().unwrap();
    // A daemon holding several workspaces routes by this identity, so it is
    // readable without resolving a scope first.
    assert_eq!(runtime.workspace_id().unwrap(), state.workspace_id);
    assert_eq!(
        runtime.resolve_root_scope(WorkspaceId::new(), runtime.root_worktree_id()),
        Err(SessionRuntimeError::ScopeUnavailable)
    );
    let create = runtime
        .handle(
            SessionAction::Create,
            &operation(),
            &json!({"name":"scope"}),
        )
        .unwrap();
    let session_id = create.body["sessions"][0]["session_id"]
        .as_str()
        .and_then(|value| SessionId::parse(value).ok())
        .unwrap();
    let scope = runtime.session_scope_by_id(session_id).unwrap();
    assert_eq!(
        scope.path,
        tmp.path().join(STATE_DIR).join(SESSIONS_DIR).join("scope")
    );
    assert_eq!(
        runtime.session_scope_by_id(SessionId::new()),
        Err(SessionRuntimeError::UnknownSession)
    );
    assert_eq!(
        runtime
            .resolve_root_scope(state.workspace_id, runtime.root_worktree_id())
            .unwrap(),
        tmp.path()
    );

    let remove_operation = operation();
    runtime
        .handle(
            SessionAction::Remove,
            &remove_operation,
            &json!({"name":"scope"}),
        )
        .unwrap();
    runtime
        .handle(
            SessionAction::Remove,
            &remove_operation,
            &json!({"name":"scope"}),
        )
        .unwrap();

    let _ = runtime
        .begin_create(
            CreateOrigin::Direct,
            &operation(),
            &json!({"name":"unfinished"}),
        )
        .unwrap();
    let current = runtime.state().unwrap();
    let journal = current.operations.last().unwrap();
    assert_eq!(
        runtime.replay(&current, journal),
        Err(SessionRuntimeError::DurableFailure(
            "session operation did not complete; explicit recovery required".into()
        ))
    );
}

// 1 つの決定表を分けると読み手が追う状態が増えるため、この関数はまとめて置く。
#[allow(clippy::too_many_lines)]
#[test]
fn status_projects_each_git_state_and_failure() {
    use ScriptedGitResult::Output;

    let tmp = tempfile::tempdir().unwrap();
    let git = ScriptedGit::new([
        Output {
            success: true,
            stdout: "",
            stderr: "",
        },
        Output {
            success: true,
            stdout: "",
            stderr: "",
        },
        Output {
            success: true,
            stdout: "",
            stderr: "",
        },
        Output {
            success: true,
            stdout: "main\n",
            stderr: "",
        },
        Output {
            success: true,
            stdout: " M file\n",
            stderr: "",
        },
        Output {
            success: true,
            stdout: "usagi/dirty\n",
            stderr: "",
        },
        Output {
            success: false,
            stdout: "",
            stderr: "",
        },
        Output {
            success: true,
            stdout: "",
            stderr: "",
        },
        Output {
            success: true,
            stdout: "usagi/synced\n",
            stderr: "",
        },
        Output {
            success: true,
            stdout: "",
            stderr: "",
        },
        Output {
            success: true,
            stdout: "",
            stderr: "",
        },
        Output {
            success: true,
            stdout: "usagi/local\n",
            stderr: "",
        },
        Output {
            success: false,
            stdout: "",
            stderr: "",
        },
    ]);
    let mut runtime = SessionRuntime::open(
        tmp.path().join("repository"),
        &tmp.path().join("daemon"),
        DaemonGeneration::new(),
        git,
        FakeSessionWorktreeIo {
            occupied: false,
            build_calls: Arc::new(AtomicUsize::new(0)),
        },
    )
    .unwrap();
    for name in ["dirty", "synced", "local"] {
        runtime
            .handle(SessionAction::Create, &operation(), &json!({"name": name}))
            .unwrap();
    }
    let reply = runtime
        .handle(SessionAction::Status, &operation(), &json!({}))
        .unwrap();
    assert!(reply.body["sessions"][0]["parent_session_id"].is_null());
    assert_eq!(reply.body["sessions"][0]["worktrees"][0]["status"], "dirty");
    assert_eq!(
        reply.body["sessions"][1]["worktrees"][0]["status"],
        "synced"
    );
    assert_eq!(reply.body["sessions"][2]["worktrees"][0]["status"], "local");

    let tmp = tempfile::tempdir().unwrap();
    let mut runtime = SessionRuntime::open(
        tmp.path().join("repository"),
        &tmp.path().join("daemon"),
        DaemonGeneration::new(),
        ScriptedGit::new([Output {
            success: false,
            stdout: "",
            stderr: "",
        }]),
        FakeSessionWorktreeIo {
            occupied: false,
            build_calls: Arc::new(AtomicUsize::new(0)),
        },
    )
    .unwrap();
    assert_eq!(
        runtime.handle(SessionAction::Status, &operation(), &json!({})),
        Err(SessionRuntimeError::Storage)
    );

    let tmp = tempfile::tempdir().unwrap();
    let mut runtime = SessionRuntime::open(
        tmp.path().join("repository"),
        &tmp.path().join("daemon"),
        DaemonGeneration::new(),
        ScriptedGit::new([
            Output {
                success: true,
                stdout: "",
                stderr: "",
            },
            Output {
                success: true,
                stdout: "main\n",
                stderr: "",
            },
            Output {
                success: false,
                stdout: "",
                stderr: "",
            },
            Output {
                success: true,
                stdout: "usagi/one\n",
                stderr: "",
            },
            Output {
                success: false,
                stdout: "",
                stderr: "",
            },
        ]),
        FakeSessionWorktreeIo {
            occupied: false,
            build_calls: Arc::new(AtomicUsize::new(0)),
        },
    )
    .unwrap();
    runtime
        .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
        .unwrap();
    assert_eq!(
        runtime.handle(SessionAction::Status, &operation(), &json!({})),
        Err(SessionRuntimeError::Storage)
    );

    let tmp = tempfile::tempdir().unwrap();
    let mut runtime = SessionRuntime::open(
        tmp.path().join("repository"),
        &tmp.path().join("daemon"),
        DaemonGeneration::new(),
        ScriptedGit::new([ScriptedGitResult::Error, ScriptedGitResult::Error]),
        FakeSessionWorktreeIo {
            occupied: false,
            build_calls: Arc::new(AtomicUsize::new(0)),
        },
    )
    .unwrap();
    assert!(matches!(
        runtime.handle(
            SessionAction::Create,
            &operation(),
            &json!({"name":"error"})
        ),
        Err(SessionRuntimeError::SessionWorkspaceCreationFailed { .. })
    ));
    assert_eq!(
        runtime.handle(SessionAction::Status, &operation(), &json!({})),
        Err(SessionRuntimeError::Storage)
    );
}

#[test]
fn unowned_reconcile_is_a_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let repository = tmp.path().join("repository");
    let state_dir = tmp.path().join("daemon");
    let mut runtime = SessionRuntime::open(
        repository,
        &state_dir,
        DaemonGeneration::new(),
        FakeSessionGit::ok(),
        SystemSessionWorktreeIo,
    )
    .unwrap();
    let mut state = runtime.state().unwrap();
    let mut unowned =
        ManagedSession::new_creating("unowned".into(), OperationId::new(), Utc::now());
    unowned.operation_id = None;
    state.sessions.push(unowned);
    let revision = state.state_revision;
    state.state_revision += 1;
    runtime.store.replace_if_revision(revision, &state).unwrap();
    runtime.reconcile().unwrap();
}

#[test]
fn shared_teardown_reports_storage_failure() {
    let (tmp, runtime) = runtime(FakeSessionGit::ok());
    let shared = Arc::new(Mutex::new(runtime));
    std::fs::write(tmp.path().join("daemon/sessions.json"), "not json").unwrap();
    let pending = PendingTeardown {
        session_id: SessionId::new(),
        operation_id: OperationId::new(),
        name: "missing".into(),
        repository_root: tmp.path().into(),
        data_home: tmp.path().into(),
        session_container: tmp.path().join(STATE_DIR).join(SESSIONS_DIR),
        session_root: tmp.path().join("missing"),
        force: false,
        delete_branch: false,
        branch_name: None,
        force_delete_branch: false,
        merged_head_oid: None,
    };
    assert_eq!(
        SharedSessionTeardown::new(shared).finish(&pending, Ok(())),
        Err("daemon could not persist session lifecycle state".into())
    );
}
