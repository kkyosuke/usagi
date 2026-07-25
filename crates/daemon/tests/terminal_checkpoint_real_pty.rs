//! Real-PTY end-to-end regression for the daemon's semantic screen checkpoint
//! (the final phase of #524).
//!
//! [`terminal_checkpoint_real_pty`] drives a **real** shell PTY through the two
//! real daemon owners — the Agent owner and the generic terminal owner — with
//! one shared fixture, so both are held to the same snapshot contract:
//!
//! * the child paints more than the 64 KiB retained journal of unique output,
//!   a long-running SGR established before that window, a saved cursor, a
//!   scroll region, and an alternate buffer that leaves a primary buffer with
//!   real scrollback saved behind it;
//! * a client attaches, disconnects, and a **fresh** client reattaches. The
//!   restored screen must equal an untrimmed reference parser fed the same
//!   bytes — visible cells, cursor, style, scroll region, the saved primary
//!   buffer and the copy history behind it — while the child PID and the spawn
//!   count stay exactly what they were (a snapshot never respawns a PTY);
//! * attach, resync, resize and the post-exit final snapshot all carry the same
//!   checkpoint payload with `base_offset == output_offset`; and
//! * the real IPC frame stays inside the default 1 MiB bound and the process
//!   retains its screen cells inside the per-terminal and aggregate budgets,
//!   releasing them when the owner is dropped.
//!
//! The legacy raw tail is measured in the same run as the counter-example: fed
//! to a blank parser it does not reproduce the reference screen and has lost the
//! history established before its window, which is precisely the shipping defect
//! the checkpoint replaces.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use usagi_core::domain::agent::{
    AgentProfile, AgentProfileId, DurableLaunchSnapshot, LaunchMode, LaunchPlan, LaunchRequest,
};
use usagi_core::domain::id::{
    ClientId, ConnectionId, DaemonGeneration, OperationId, RequestId, SessionId, TerminalRef,
    WorkspaceId, WorktreeId,
};
use usagi_core::domain::terminal_launch::{
    DurableTerminalLaunchSnapshot, ResolvedTerminalLaunch, TerminalLaunchRequest,
    TerminalLaunchScope, TerminalLaunchValidationError, TerminalProfileId,
};
use usagi_core::infrastructure::ipc::{DEFAULT_MAX_FRAME_BYTES, write_json_frame};
use usagi_core::usecase::agent::AgentProfileCatalog;
use usagi_core::usecase::client::{
    AgentLaunchIntent, TerminalAction, TerminalGeometry, TerminalLaunchIntent, TerminalRequest,
};
use usagi_core::usecase::vt_screen::{
    ActiveBuffer, Geometry as ScreenGeometry, RowCheckpoint, ScreenCheckpoint, VtScreen,
};
use usagi_daemon::infrastructure::pty::PtyTerminal;
use usagi_daemon::presentation::ipc::TerminalOwner;
use usagi_daemon::usecase::agent_ipc::{
    AgentRuntime, AgentTerminalActor, ResolvedAgentScope, ScopeResolveError, SessionScopeResolver,
    TerminalOutcome,
};
use usagi_daemon::usecase::generation::ProcessIdentity;
use usagi_daemon::usecase::generic_terminal::{
    GenericPtySpawner, TerminalProfileResolver, TerminalStore, TerminalStoreSnapshot,
};
use usagi_daemon::usecase::orchestration::AdapterRegistry;
use usagi_daemon::usecase::runtime::{
    AdapterError, AgentAdapter, OutputJournal, PtySpawner, ResolvedLaunch, RuntimeStore,
    RuntimeStoreSnapshot, SpawnProvision,
};
use usagi_daemon::usecase::terminal::{
    Geometry, MAX_RETAINED_OUTPUT_BYTES, Output, OutputPipelineCounters, PtyWriteError, PtyWriter,
    SCREEN_CELLS_AGGREGATE_MAX, SCREEN_CELLS_PER_TERMINAL_MAX, SnapshotWire, SpawnFailure,
    output_pipeline_counters,
};
use usagi_daemon::usecase::terminal_ipc::{
    GenericTerminalRuntime, ResolvedTerminalScope, TerminalScopeResolveError, TerminalScopeResolver,
};

// ---- the painted screen -----------------------------------------------------

/// The geometry both owners launch at, and the one the reference parser uses.
const GEOMETRY: Geometry = Geometry { cols: 80, rows: 24 };
/// The geometry the resize contract commits to mid-session.
const RESIZED: Geometry = Geometry {
    cols: 100,
    rows: 30,
};
/// Unique scrollback lines. Each is 70 columns wide, so the earliest ones fall
/// out of the 64 KiB journal while the screen keeps them.
const UNIQUE_LINES: usize = 600;
/// Row-overwriting progress ticks: bytes without rows, so the stream outgrows
/// the journal without growing the checkpoint.
const PROGRESS_TICKS: usize = 900;
/// Compressible run inside each unique line.
const FILL: &str = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
/// Compressible run inside each progress tick.
const BAR: &str = "##################################################";
/// Printed once, last, so the fixture knows the child has painted everything.
const SENTINEL: &[u8] = b"CHECKPOINT-READY";
/// The exit status the child commits after the client releases it.
const EXIT_STATUS: i32 = 7;
/// The first unique line: printed before the retained journal window begins, so
/// only a semantic checkpoint can still restore it.
const FIRST_LINE: &str = "line-0000";

/// The shell program both owners resolve.
///
/// It uses shell builtins only (no `PATH`), establishes state *before* the
/// journal window, and then blocks on `read` so the reattach assertions run
/// against a live child.
fn script() -> String {
    format!(
        concat!(
            // A long-running SGR: set once, never reset, established far before
            // the retained journal window begins.
            "printf '\\033[1;4;31m';",
            // Unique scrollback the bounded journal cannot keep.
            "i=0; while [ $i -lt {lines} ]; do printf 'line-%04d {fill}\\n' $i; i=$((i+1)); done;",
            // Save the cursor (SCP), then move away from the saved point.
            "printf '\\033[s\\033[5;10H';",
            // Overwrite one row: bytes without rows.
            "j=0; while [ $j -lt {ticks} ]; do printf '\\rprogress %04d {bar}' $j; j=$((j+1)); done;",
            // Enter the alternate buffer, reserve a scroll region, paint it. The
            // primary buffer and its scrollback are now saved behind it.
            "printf '\\033[?1049h\\033[2;20r';",
            "printf 'alt-header\\r\\n\\033[3;5Halt-body';",
            "printf '{sentinel}';",
            // Block until the client writes one line through the daemon. The
            // bound keeps a spurious interrupt from spinning.
            "k=0; while [ $k -lt 100 ]; do read line && break; k=$((k+1)); done;",
            // Leave the alternate buffer and exit with a known status.
            "printf '\\033[?1049l'; exit {status}"
        ),
        lines = UNIQUE_LINES,
        ticks = PROGRESS_TICKS,
        fill = FILL,
        bar = BAR,
        sentinel = std::str::from_utf8(SENTINEL).expect("the sentinel is ASCII"),
        status = EXIT_STATUS,
    )
}

// ---- the real PTY -----------------------------------------------------------

/// What the PTY reader thread observes on the master.
enum Observation {
    Output(Vec<u8>),
    Exited(i32),
}

/// One PTY this fixture opened, kept so a later phase can prove the child was
/// never replaced.
struct SpawnRecord {
    terminal_id: String,
    pid: u32,
    pty: Arc<Mutex<PtyTerminal>>,
}

/// Every PTY this spawner ever opened, so the fixture can prove that a snapshot
/// never adds one.
#[derive(Clone, Default)]
struct PtyLedger(Arc<Mutex<Vec<SpawnRecord>>>);

impl PtyLedger {
    fn record(&self, spawned: SpawnRecord) {
        self.0
            .lock()
            .expect("the PTY ledger lock is healthy")
            .push(spawned);
    }
    fn spawns(&self) -> usize {
        self.0.lock().expect("the PTY ledger lock is healthy").len()
    }
    /// The PID of the only child this fixture spawns.
    fn pid(&self) -> u32 {
        self.0.lock().expect("the PTY ledger lock is healthy")[0].pid
    }
    fn pty(&self, terminal_id: &str) -> Arc<Mutex<PtyTerminal>> {
        let ledger = self.0.lock().expect("the PTY ledger lock is healthy");
        let spawned = ledger
            .iter()
            .find(|spawned| spawned.terminal_id == terminal_id)
            .expect("the addressed terminal was spawned by this fixture");
        Arc::clone(&spawned.pty)
    }
}

/// Opens actual pseudo-terminals for whichever owner resolved the launch, and
/// streams the master into an observation channel.
struct RealPty {
    observations: Sender<Observation>,
    ledger: PtyLedger,
    /// The Agent owner spawns without a geometry parameter; it launches every
    /// terminal at the runtime geometry.
    geometry: Geometry,
    /// The terminal selected for the next input write.
    selected: Option<String>,
}

impl RealPty {
    fn open(
        &mut self,
        terminal: &TerminalRef,
        program: &str,
        args: &[String],
        environment: &[(String, String)],
        directory: &Path,
        geometry: Geometry,
    ) -> ProcessIdentity {
        let pty = PtyTerminal::spawn_with(program, args, environment, directory, geometry)
            .expect("the test shell PTY opens");
        let pid = pty
            .process_id()
            .expect("a freshly spawned PTY reports its child PID");
        let reader = pty.reader().expect("the PTY master can be read");
        let pty = Arc::new(Mutex::new(pty));
        self.ledger.record(SpawnRecord {
            terminal_id: terminal.terminal_id.as_str().clone(),
            pid,
            pty: Arc::clone(&pty),
        });
        let observations = self.observations.clone();
        std::thread::spawn(move || pump(reader, &pty, &observations));
        ProcessIdentity {
            pid,
            start_identity: "real-pty".to_owned(),
            process_group: pid,
        }
    }

    fn selected_pty(&self) -> Arc<Mutex<PtyTerminal>> {
        let selected = self
            .selected
            .as_deref()
            .expect("input selects a terminal before writing");
        self.ledger.pty(selected)
    }
}

/// Drains one PTY master to EOF, then reaps the child for its real exit status.
fn pump(
    mut reader: Box<dyn Read + Send>,
    pty: &Arc<Mutex<PtyTerminal>>,
    observations: &Sender<Observation>,
) {
    let mut bytes = [0_u8; 4096];
    loop {
        // EOF and a closed master both end the stream; the child status is then
        // read from the reaped child rather than guessed.
        let count = reader.read(&mut bytes).unwrap_or(0);
        if count == 0 {
            break;
        }
        let _ = observations.send(Observation::Output(bytes[..count].to_vec()));
    }
    let status = pty
        .lock()
        .expect("the PTY lock is healthy")
        .wait()
        .expect("the real child reports an exit status");
    let _ = observations.send(Observation::Exited(status));
}

impl PtySpawner for RealPty {
    fn spawn(
        &mut self,
        launch: &DurableLaunchSnapshot,
        provision: &SpawnProvision,
        terminal: &TerminalRef,
    ) -> Result<ProcessIdentity, SpawnFailure> {
        let plan = &launch.plan;
        let mut argv = plan.argv.clone();
        argv.extend(provision.arguments().iter().cloned());
        let environment = provision.compose_environment(&BTreeMap::new());
        let geometry = self.geometry;
        Ok(self.open(
            terminal,
            &plan.program,
            &argv,
            &environment.into_iter().collect::<Vec<_>>(),
            &plan.working_directory,
            geometry,
        ))
    }
}

impl GenericPtySpawner for RealPty {
    fn spawn(
        &mut self,
        launch: &ResolvedTerminalLaunch,
        terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<ProcessIdentity, SpawnFailure> {
        // The shell profile resolves no environment values; the script needs
        // only builtins, so the child runs with a cleared environment.
        Ok(self.open(
            terminal,
            &launch.snapshot.program,
            &launch.snapshot.arguments,
            &[],
            &launch.snapshot.working_directory,
            geometry,
        ))
    }
}

impl PtyWriter for RealPty {
    fn select_terminal(&mut self, terminal: &TerminalRef) {
        self.selected = Some(terminal.terminal_id.as_str().clone());
    }

    fn resize(&mut self, terminal: &TerminalRef, geometry: Geometry) -> Result<(), PtyWriteError> {
        self.ledger
            .pty(terminal.terminal_id.as_str().as_str())
            .lock()
            .expect("the PTY lock is healthy")
            .resize(geometry)
            .map_err(|_| PtyWriteError { applied_prefix: 0 })
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), PtyWriteError> {
        self.selected_pty()
            .lock()
            .expect("the PTY lock is healthy")
            .write_all(bytes)
    }

    fn release(&mut self, _terminal: &TerminalRef) -> bool {
        true
    }
}

// ---- owner-side fakes that are not the subject of this regression -----------

struct MemoryStore;
impl RuntimeStore for MemoryStore {
    fn save(&mut self, _: RuntimeStoreSnapshot) -> Result<(), ()> {
        Ok(())
    }
}

struct MemoryJournal;
impl OutputJournal for MemoryJournal {
    fn append(&mut self, _: &Output) -> Result<(), ()> {
        Ok(())
    }
}

struct MemoryTerminalStore;
impl TerminalStore for MemoryTerminalStore {
    fn save(&mut self, _: TerminalStoreSnapshot) -> Result<(), ()> {
        Ok(())
    }
}

/// Resolves the Agent scope to a fixed available worktree.
struct FixedAgentScope {
    worktree_id: WorktreeId,
    working_directory: PathBuf,
}
impl SessionScopeResolver for FixedAgentScope {
    fn resolve_available_scope(
        &self,
        _: WorkspaceId,
        _: Option<SessionId>,
    ) -> Result<ResolvedAgentScope, ScopeResolveError> {
        Ok(ResolvedAgentScope {
            worktree_id: self.worktree_id,
            working_directory: self.working_directory.clone(),
        })
    }
}

/// Resolves the generic terminal scope to a fixed available worktree.
struct FixedTerminalScope {
    scope: TerminalLaunchScope,
    working_directory: PathBuf,
}
impl TerminalScopeResolver for FixedTerminalScope {
    fn resolve_available_scope(
        &self,
        _: &TerminalLaunchScope,
    ) -> Result<ResolvedTerminalScope, TerminalScopeResolveError> {
        Ok(ResolvedTerminalScope {
            scope: self.scope.clone(),
            working_directory: self.working_directory.clone(),
        })
    }
}

/// Renders the painting script into the Agent owner's durable launch plan, so
/// the regression depends on no product binary being installed.
struct ShellAdapter {
    profile: AgentProfile,
}
impl AgentProfileCatalog for ShellAdapter {
    fn find(&self, id: &AgentProfileId) -> Option<AgentProfile> {
        (id == &self.profile.id).then(|| self.profile.clone())
    }
}
impl AgentAdapter for ShellAdapter {
    fn resolve(&mut self, request: &LaunchRequest) -> Result<ResolvedLaunch, AdapterError> {
        let plan = LaunchPlan::new(
            request.profile_id.clone(),
            self.profile.revision,
            "/bin/sh",
            vec!["-c".to_owned(), script()],
            [],
            PathBuf::from("/"),
        )
        .expect("the shell plan is valid");
        Ok(ResolvedLaunch {
            snapshot: DurableLaunchSnapshot::new(request.clone(), plan),
            provision: SpawnProvision::new([], Vec::new()),
            provider_resume: request.provider_resume.clone(),
        })
    }
}

/// Renders the painting script into the generic owner's durable launch snapshot.
struct ShellProfile;
impl TerminalProfileResolver for ShellProfile {
    fn resolve(
        &mut self,
        request: &TerminalLaunchRequest,
    ) -> Result<ResolvedTerminalLaunch, TerminalLaunchValidationError> {
        ResolvedTerminalLaunch::new(
            DurableTerminalLaunchSnapshot::new(
                request.clone(),
                1,
                "/bin/sh",
                vec!["-c".to_owned(), script()],
                PathBuf::from("/"),
                [],
            )?,
            BTreeMap::new(),
        )
    }
}

// ---- the owner under test ---------------------------------------------------

/// The daemon-side owner of a terminal, so one fixture drives the Agent owner
/// and the generic terminal owner through the same vocabulary.
trait DaemonOwner {
    fn request(
        &mut self,
        connection: ConnectionId,
        client: ClientId,
        action: TerminalAction,
        request: TerminalRequest,
        wire: SnapshotWire,
    ) -> Value;
    fn output(&mut self, terminal: &TerminalRef, bytes: Vec<u8>);
    fn exit(&mut self, terminal: &TerminalRef, status: i32);
    fn disconnect(&mut self, connection: ConnectionId);
}

struct AgentOwner(AgentRuntime);
impl DaemonOwner for AgentOwner {
    fn request(
        &mut self,
        connection: ConnectionId,
        client: ClientId,
        action: TerminalAction,
        request: TerminalRequest,
        wire: SnapshotWire,
    ) -> Value {
        let outcome =
            self.0
                .handle_terminal(connection, client, RequestId::new(), action, request, wire);
        match outcome {
            TerminalOutcome::Handled(result) => {
                result.expect("the Agent owner completes its terminal request")
            }
            TerminalOutcome::NotOwned => panic!("the Agent owner owns the terminal it launched"),
        }
    }
    fn output(&mut self, terminal: &TerminalRef, bytes: Vec<u8>) {
        self.0
            .output(terminal, bytes)
            .expect("the Agent owner journals its PTY output");
    }
    fn exit(&mut self, terminal: &TerminalRef, status: i32) {
        self.0
            .exit(terminal, status)
            .expect("the Agent owner commits its PTY exit");
    }
    fn disconnect(&mut self, connection: ConnectionId) {
        AgentTerminalActor::disconnect(&mut self.0, connection);
    }
}

type GenericOwner =
    GenericTerminalRuntime<ShellProfile, MemoryTerminalStore, RealPty, FixedTerminalScope>;

impl DaemonOwner for GenericOwner {
    fn request(
        &mut self,
        connection: ConnectionId,
        client: ClientId,
        action: TerminalAction,
        request: TerminalRequest,
        wire: SnapshotWire,
    ) -> Value {
        TerminalOwner::request(
            self,
            connection,
            client,
            RequestId::new(),
            action,
            serde_json::to_value(request).expect("the terminal request vocabulary serializes"),
            wire,
        )
        .expect("the generic owner completes its terminal request")
    }
    fn output(&mut self, terminal: &TerminalRef, bytes: Vec<u8>) {
        GenericTerminalRuntime::output(self, terminal, bytes)
            .expect("the generic owner journals its PTY output");
    }
    fn exit(&mut self, terminal: &TerminalRef, status: i32) {
        GenericTerminalRuntime::exit(self, terminal, status)
            .expect("the generic owner commits its PTY exit");
    }
    fn disconnect(&mut self, connection: ConnectionId) {
        TerminalOwner::disconnect(self, connection);
    }
}

// ---- fixture ----------------------------------------------------------------

/// One launched real-PTY terminal behind a daemon owner.
struct Scenario<O> {
    owner: O,
    terminal: TerminalRef,
    ledger: PtyLedger,
    observations: Receiver<Observation>,
    /// Retention counters sampled before this scenario reserved anything.
    baseline: OutputPipelineCounters,
}

/// How far [`drain`] feeds the real PTY into the daemon.
#[derive(Clone, Copy)]
enum Until<'a> {
    /// Until this marker appears in the accumulated stream.
    Sentinel(&'a [u8]),
    /// Until the child's exit has been committed.
    Exit,
}

/// Feeds real PTY observations into the daemon owner, accumulating the exact
/// byte stream the reference parser is fed. Returns the committed exit status
/// once the child ended.
fn drain<O: DaemonOwner>(
    owner: &mut O,
    terminal: &TerminalRef,
    observations: &Receiver<Observation>,
    raw: &mut Vec<u8>,
    until: Until<'_>,
) -> Option<i32> {
    loop {
        let observation = observations
            .recv_timeout(Duration::from_secs(60))
            .expect("the real PTY produces its next observation before the timeout");
        match observation {
            Observation::Output(bytes) => {
                raw.extend_from_slice(&bytes);
                owner.output(terminal, bytes);
                if let Until::Sentinel(marker) = until {
                    let tail = raw.len().saturating_sub(8 * 1024);
                    if raw[tail..]
                        .windows(marker.len())
                        .any(|slice| slice == marker)
                    {
                        return None;
                    }
                }
            }
            Observation::Exited(status) => {
                owner.exit(terminal, status);
                return Some(status);
            }
        }
    }
}

fn agent_scenario() -> Scenario<AgentOwner> {
    let (observations_tx, observations) = mpsc::channel();
    let ledger = PtyLedger::default();
    let profile = AgentProfile::new(
        AgentProfileId::new("claude").expect("the profile ID is canonical"),
        "Claude",
        1,
        [],
        [LaunchMode::Interactive],
    );
    let mut registry = AdapterRegistry::new();
    registry
        .register(
            profile.clone(),
            Box::new(ShellAdapter {
                profile: profile.clone(),
            }),
        )
        .expect("the shell adapter registers");
    let baseline = output_pipeline_counters();
    let mut runtime = AgentRuntime::new(
        DaemonGeneration::new(),
        registry,
        MemoryStore,
        MemoryJournal,
        RealPty {
            observations: observations_tx,
            ledger: ledger.clone(),
            geometry: GEOMETRY,
            selected: None,
        },
        profile.id.clone(),
        GEOMETRY,
    );
    let admission = runtime
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace: WorkspaceId::new(),
                session: Some(SessionId::new()),
                profile: None,
            },
            &FixedAgentScope {
                worktree_id: WorktreeId::new(),
                working_directory: PathBuf::from("/"),
            },
        )
        .expect("the Agent owner admits the real PTY launch");
    Scenario {
        terminal: admission.terminal.clone(),
        owner: AgentOwner(runtime),
        ledger,
        observations,
        baseline,
    }
}

fn generic_scenario() -> Scenario<GenericOwner> {
    let (observations_tx, observations) = mpsc::channel();
    let ledger = PtyLedger::default();
    let scope = TerminalLaunchScope {
        workspace_id: WorkspaceId::new(),
        session_id: Some(SessionId::new()),
        worktree_id: WorktreeId::new(),
    };
    let baseline = output_pipeline_counters();
    let mut runtime = GenericTerminalRuntime::new(
        DaemonGeneration::new(),
        ShellProfile,
        MemoryTerminalStore,
        RealPty {
            observations: observations_tx,
            ledger: ledger.clone(),
            geometry: GEOMETRY,
            selected: None,
        },
        FixedTerminalScope {
            scope: scope.clone(),
            working_directory: PathBuf::from("/"),
        },
    );
    let launched = DaemonOwner::request(
        &mut runtime,
        ConnectionId::new(),
        ClientId::new(),
        TerminalAction::Launch,
        TerminalRequest::Launch {
            intent: TerminalLaunchIntent {
                request: TerminalLaunchRequest {
                    profile_id: TerminalProfileId::new("shell")
                        .expect("the profile ID is canonical"),
                    scope,
                },
                geometry: TerminalGeometry {
                    cols: GEOMETRY.cols,
                    rows: GEOMETRY.rows,
                },
            },
        },
        SnapshotWire::ScreenCheckpoint,
    );
    Scenario {
        terminal: serde_json::from_value(launched["terminal"].clone())
            .expect("the launch response carries the fenced terminal"),
        owner: runtime,
        ledger,
        observations,
        baseline,
    }
}

// ---- assertions shared by both owners ---------------------------------------

/// The framed size of one real IPC response, length prefix included.
fn framed_bytes(response: &Value) -> usize {
    let mut frame = Vec::new();
    write_json_frame(&mut frame, response, DEFAULT_MAX_FRAME_BYTES)
        .expect("the snapshot response fits the default frame bound");
    frame.len()
}

/// The semantic checkpoint carried by one snapshot payload.
fn checkpoint_of(snapshot: &Value) -> ScreenCheckpoint {
    serde_json::from_value(snapshot["screen"].clone())
        .expect("a revision 2 snapshot carries a screen checkpoint")
}

/// Expands run-length rows back into plain text so a checkpoint's own history
/// can be read without reconstructing a parser.
fn expand(rows: &[RowCheckpoint]) -> Vec<String> {
    rows.iter()
        .map(|row| {
            row.runs
                .iter()
                .flat_map(|run| std::iter::repeat_n(run.ch, run.repeat as usize))
                .collect()
        })
        .collect()
}

fn holds_first_line(rows: &[RowCheckpoint]) -> bool {
    expand(rows).iter().any(|row| row.starts_with(FIRST_LINE))
}

fn attach<O: DaemonOwner>(
    owner: &mut O,
    terminal: &TerminalRef,
    connection: ConnectionId,
    client: ClientId,
) -> Value {
    owner.request(
        connection,
        client,
        TerminalAction::Attach,
        TerminalRequest::Attach {
            terminal: terminal.clone(),
        },
        SnapshotWire::ScreenCheckpoint,
    )
}

fn resync<O: DaemonOwner>(
    owner: &mut O,
    terminal: &TerminalRef,
    connection: ConnectionId,
    client: ClientId,
    wire: SnapshotWire,
) -> Value {
    owner.request(
        connection,
        client,
        TerminalAction::Resync,
        TerminalRequest::Resync {
            terminal: terminal.clone(),
        },
        wire,
    )
}

/// The whole checkpoint contract against one real daemon owner and one real PTY.
#[allow(clippy::too_many_lines)] // One end-to-end scenario, kept in reading order.
fn checkpoint_contract<O: DaemonOwner>(scenario: Scenario<O>, label: &str) {
    let Scenario {
        mut owner,
        terminal,
        ledger,
        observations,
        baseline,
    } = scenario;

    // 1. Let the child paint everything, feeding the daemon exactly what the
    //    reference parser will see.
    let mut raw = Vec::new();
    let running = drain(
        &mut owner,
        &terminal,
        &observations,
        &mut raw,
        Until::Sentinel(SENTINEL),
    );
    assert_eq!(running, None, "{label}: the child is still live");
    let painted = raw.len();
    assert!(
        painted > MAX_RETAINED_OUTPUT_BYTES,
        "{label}: the child outgrew the retained journal ({painted} bytes)"
    );
    assert_eq!(ledger.spawns(), 1, "{label}: exactly one PTY was spawned");
    let pid = ledger.pid();

    // The untrimmed authority: a parser that saw every byte, never a window.
    let mut reference = VtScreen::new(usize::from(GEOMETRY.rows), usize::from(GEOMETRY.cols));
    reference.advance(&raw);

    // 2. The first client attaches on the negotiated checkpoint wire.
    let first = ConnectionId::new();
    let first_client = ClientId::new();
    let attached = attach(&mut owner, &terminal, first, first_client);
    let frame = framed_bytes(&attached);
    assert!(
        frame <= DEFAULT_MAX_FRAME_BYTES,
        "{label}: the attach frame stays inside the default bound ({frame} bytes)"
    );
    let snapshot = &attached["snapshot"];
    assert_eq!(
        snapshot["base_offset"], snapshot["output_offset"],
        "{label}: a checkpoint is complete at its output offset"
    );
    assert_eq!(snapshot["output_offset"], json!(painted));
    assert_eq!(snapshot["revision"], json!(0));
    assert_eq!(snapshot["exited"], Value::Null);
    let checkpoint = checkpoint_of(snapshot);
    let restored = VtScreen::from_checkpoint(&checkpoint)
        .expect("the daemon's own checkpoint decodes inside every bound");
    assert_eq!(
        restored, reference,
        "{label}: the restored screen equals the untrimmed reference"
    );
    assert_eq!(restored.checkpoint(), checkpoint);

    // The alternate buffer is live, so the primary is the saved background and
    // keeps the copy history the journal already dropped.
    assert_eq!(checkpoint.active, ActiveBuffer::Alternate);
    assert!(checkpoint.alternate.is_some());
    assert!(
        holds_first_line(&checkpoint.primary.scrollback),
        "{label}: the saved primary buffer keeps pre-window history"
    );
    assert!(checkpoint.primary.saved_cursor.is_some());
    assert_eq!(
        restored.cells_with_scrollback(),
        reference.cells_with_scrollback(),
        "{label}: the copy history matches the reference"
    );

    // The legacy raw tail is the counter-example: cut at an arbitrary boundary it
    // reproduces neither the screen nor the history established before it.
    let legacy = resync(
        &mut owner,
        &terminal,
        first,
        first_client,
        SnapshotWire::RawTail,
    );
    let tail: Vec<u8> = serde_json::from_value(legacy["replay"].clone())
        .expect("a revision 1 snapshot carries a raw tail");
    assert_eq!(tail.len(), MAX_RETAINED_OUTPUT_BYTES);
    let mut blank = VtScreen::new(usize::from(GEOMETRY.rows), usize::from(GEOMETRY.cols));
    blank.advance(&tail);
    assert_ne!(
        blank, reference,
        "{label}: the raw tail cannot reproduce the reference screen"
    );
    let tail_history = blank.checkpoint().primary.scrollback;
    assert!(
        !holds_first_line(&tail_history),
        "{label}: the raw tail lost the history established before its window"
    );
    assert!(
        tail_history.len() < checkpoint.primary.scrollback.len(),
        "{label}: the raw tail recovers less history than the checkpoint"
    );

    // 3. The client disconnects and a *fresh* client reattaches. The screen is
    //    identical and the child was never respawned.
    owner.disconnect(first);
    let second = ConnectionId::new();
    let client = ClientId::new();
    let reattached = attach(&mut owner, &terminal, second, client);
    assert_ne!(
        reattached["subscription"], attached["subscription"],
        "{label}: a fresh connection takes its own subscription"
    );
    let reattached_checkpoint = checkpoint_of(&reattached["snapshot"]);
    assert_eq!(
        reattached_checkpoint, checkpoint,
        "{label}: reattach hands out the same screen"
    );
    assert_eq!(
        VtScreen::from_checkpoint(&reattached_checkpoint).expect("the reattach checkpoint decodes"),
        reference
    );
    assert_eq!(ledger.spawns(), 1, "{label}: reattach never respawns a PTY");
    assert_eq!(ledger.pid(), pid, "{label}: the child PID is unchanged");

    // 4. Resync carries the same payload under the same offset contract.
    let resynced = resync(
        &mut owner,
        &terminal,
        second,
        client,
        SnapshotWire::ScreenCheckpoint,
    );
    assert_eq!(resynced["base_offset"], resynced["output_offset"]);
    assert_eq!(resynced["output_offset"], json!(painted));
    assert_eq!(checkpoint_of(&resynced), checkpoint);

    // 5. Resize commits a new geometry and revision, and reshapes the decoded
    //    cells exactly as the reference does.
    let resized = owner.request(
        second,
        client,
        TerminalAction::Resize,
        TerminalRequest::Resize {
            terminal: terminal.clone(),
            geometry: TerminalGeometry {
                cols: RESIZED.cols,
                rows: RESIZED.rows,
            },
        },
        SnapshotWire::ScreenCheckpoint,
    );
    assert_eq!(
        resized["geometry"],
        json!({"cols": RESIZED.cols, "rows": RESIZED.rows})
    );
    assert_eq!(resized["revision"], json!(1));
    assert_eq!(resized["base_offset"], resized["output_offset"]);
    reference.resize(usize::from(RESIZED.rows), usize::from(RESIZED.cols));
    let resized_checkpoint = checkpoint_of(&resized);
    assert_eq!(
        resized_checkpoint.geometry,
        ScreenGeometry {
            rows: u32::from(RESIZED.rows),
            cols: u32::from(RESIZED.cols),
        }
    );
    assert_eq!(
        VtScreen::from_checkpoint(&resized_checkpoint).expect("the resized checkpoint decodes"),
        reference,
        "{label}: resize reshapes the screen, it does not replay control bytes"
    );

    // 6. Release the child through the real input path and let it exit.
    let subscription = reattached["subscription"]
        .as_u64()
        .expect("attach returns a subscription");
    let ack = owner.request(
        second,
        client,
        TerminalAction::Input,
        TerminalRequest::Input {
            terminal: terminal.clone(),
            subscription,
            input_seq: 0,
            input_operation: None,
            bytes: b"\n".to_vec(),
        },
        SnapshotWire::ScreenCheckpoint,
    );
    assert_eq!(ack["ack"], json!("Written"));
    let status = drain(&mut owner, &terminal, &observations, &mut raw, Until::Exit);
    assert_eq!(status, Some(EXIT_STATUS), "{label}: the real child exited");
    reference.advance(&raw[painted..]);

    // 7. The exit final snapshot uses the same contract, and the primary
    //    history survived the alternate round trip.
    let final_frame = resync(
        &mut owner,
        &terminal,
        second,
        client,
        SnapshotWire::ScreenCheckpoint,
    );
    assert_eq!(final_frame["exited"], json!(EXIT_STATUS));
    assert_eq!(final_frame["revision"], json!(2));
    assert_eq!(final_frame["base_offset"], final_frame["output_offset"]);
    assert_eq!(final_frame["output_offset"], json!(raw.len()));
    let final_checkpoint = checkpoint_of(&final_frame);
    assert_eq!(final_checkpoint.active, ActiveBuffer::Primary);
    assert!(final_checkpoint.alternate.is_none());
    assert!(
        holds_first_line(&final_checkpoint.primary.scrollback),
        "{label}: leaving the alternate buffer restores the primary history"
    );
    assert_eq!(
        VtScreen::from_checkpoint(&final_checkpoint).expect("the final checkpoint decodes"),
        reference,
        "{label}: the final snapshot equals the untrimmed reference"
    );
    assert_eq!(ledger.spawns(), 1, "{label}: no snapshot ever respawned");
    assert_eq!(ledger.pid(), pid);

    // 8. Bounds actually measured on this real stream: the journal dropped
    //    history, the checkpoint kept all of it, and the retained cells stayed
    //    inside both budgets.
    let counters = output_pipeline_counters();
    assert!(
        counters.dropped_bytes > baseline.dropped_bytes,
        "{label}: the bounded journal dropped output the checkpoint still carries"
    );
    assert_eq!(
        counters.screen_trimmed_rows, baseline.screen_trimmed_rows,
        "{label}: the screen stayed inside its cell budget"
    );
    assert_eq!(
        counters.checkpoint_trimmed_rows, baseline.checkpoint_trimmed_rows,
        "{label}: the checkpoint fit the real frame without dropping history"
    );
    let retained = counters.retained_screen_cells - baseline.retained_screen_cells;
    assert!(retained > 0);
    assert!(
        retained <= SCREEN_CELLS_PER_TERMINAL_MAX as u64,
        "{label}: one terminal retains at most its own budget ({retained} cells)"
    );
    assert!(counters.retained_screen_cells <= SCREEN_CELLS_AGGREGATE_MAX as u64);

    // Dropping the owner returns every retained cell to the process budget.
    drop(owner);
    assert_eq!(
        output_pipeline_counters().retained_screen_cells,
        baseline.retained_screen_cells,
        "{label}: dropping the owner releases its screen"
    );
}

/// Both real daemon owners are held to one checkpoint contract, in one test so
/// the process-local retention counters stay deterministic.
#[test]
fn terminal_checkpoint_real_pty() {
    checkpoint_contract(generic_scenario(), "generic");
    checkpoint_contract(agent_scenario(), "agent");
}
