//! The one shipping path a planned daemon replacement takes.
//!
//! `usagi daemon restart` and the build/update trigger of `usagi daemon replace`
//! are the same lifecycle event — replace the daemon that holds authority — so
//! they converge here instead of each calling [`stop`](crate::usecase::stop) and
//! [`start`](crate::usecase::start) on their own. One entry point is what makes
//! the guard below impossible to bypass.
//!
//! Two facts decide what a replacement may do, and both are observed rather
//! than assumed:
//!
//! | observation | port |
//! |---|---|
//! | how much live runtime this daemon still owns | [`ResourceCensus`] |
//! | why this build cannot hand authority to a live successor | [`seamless_refusal`] |
//!
//! A *seamless* rollover keeps the old process alive as a draining generation so
//! its PTYs survive the replacement. The synthesis root stages a standby and
//! asks the old active over IPC to drive its own process-local barrier.
//!
//! A cold transition destroys every PTY the old daemon owns. It is used when
//! the daemon owns none or when the operator explicitly gives them up; otherwise
//! the planned path is seamless:
//!
//! ```text
//! live runtime = 0            -> cold transition
//! live runtime > 0, planned   -> seamless rollover when the registry permits it
//! live runtime > 0, cold      -> cold transition, explicitly asked for
//! ```
//!
//! `daemon stop` is deliberately a different contract ([`plan_stop`]): stopping
//! is not a rollover, so it has no seamless alternative to report — it only
//! refuses to destroy live runtime that was not explicitly given up.
//!
//! The contract is documented in
//! [5. daemon](../../../../document/05-daemon.md#planned-replacement).

use std::fmt;
use std::io;

use usagi_core::domain::AppInfo;
use usagi_core::domain::daemon::DaemonState;
use usagi_core::infrastructure::daemon::{
    DaemonLauncher, DaemonRecordStore, LivenessProbe, RecordFile, Sleeper, Terminator,
};
use usagi_core::infrastructure::ipc::{BuildIdentity, OperationId, build_rollover_trigger};

use crate::usecase::authority::registry::{REGISTRY_SCHEMA, RegistryDocument};
use crate::usecase::stop::StaleDaemonCleanup;
use crate::usecase::terminal::TerminalRuntimeState;
use crate::usecase::{restart, stop};

/// How much live runtime a daemon still owns.
///
/// "Live" is exactly the two states in which the daemon holds the PTY master:
/// a reserved slot whose spawn is in flight, and a running child. A record that
/// a previous crash left needing reconciliation is *not* live — its owner is
/// already gone, so a transition cannot take anything away from it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiveResources {
    /// Agent runtimes (`agents.json`) this daemon owns.
    pub agents: usize,
    /// Generic terminals (`terminals.json`) this daemon owns.
    pub terminals: usize,
}

impl LiveResources {
    /// Every owned runtime, whichever kind it is.
    #[must_use]
    pub const fn total(&self) -> usize {
        self.agents + self.terminals
    }

    /// Whether this daemon owns nothing a transition could destroy.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

impl fmt::Display for LiveResources {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} Agent runtime(s) and {} generic terminal(s)",
            self.agents, self.terminals
        )
    }
}

/// Whether this durable state means the daemon holds the PTY master.
///
/// A record left needing reconciliation is deliberately *not* live: its owner
/// is already gone, so no transition can take anything from it. Exactly the two
/// states below are the ones a cold transition would destroy.
#[must_use]
pub const fn owns_pty(state: TerminalRuntimeState) -> bool {
    matches!(
        state,
        TerminalRuntimeState::Reserved | TerminalRuntimeState::Running
    )
}

/// Count the live runtime behind the two durable snapshots' record states.
///
/// The snapshots are the daemon's own single-writer state, so a lifecycle verb
/// taking this census reads exactly what the running daemon last committed.
#[must_use]
pub fn census_of(
    agents: &[TerminalRuntimeState],
    terminals: &[TerminalRuntimeState],
) -> LiveResources {
    let mut live = LiveResources::default();
    for state in agents {
        live.agents += usize::from(owns_pty(*state));
    }
    for state in terminals {
        live.terminals += usize::from(owns_pty(*state));
    }
    live
}

/// Reading how much live runtime the recorded daemon owns.
///
/// It is a port because the durable runtime stores are the composition root's
/// to bind: this usecase only needs the two counts, and a test supplies them
/// directly.
pub trait ResourceCensus {
    /// Count the live runtime of the daemon that owns this data directory.
    ///
    /// # Errors
    /// Returns the durable store's read error. A census that cannot be taken is
    /// never treated as "nothing is live".
    fn live(&self) -> io::Result<LiveResources>;
}

/// Why this build cannot hand authority to a live successor.
///
/// Every variant is a statement about the durable generation registry, so the
/// message an operator sees names the prerequisite that is actually missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeamlessRefusal {
    /// No durable registry exists: this daemon has never registered a
    /// generation, so there is no successor to hand authority to.
    NoGenerationRegistry,
    /// The registry exists but is not the schema this build writes.
    RegistrySchemaUnsupported,
    /// The registry cannot be read or does not parse. Fail closed rather than
    /// guess at an authority.
    RegistryUnreadable(String),
    /// The registry does not name a live registered active generation.
    NoLiveRegisteredActive,
    /// No additional retained generation can be staged.
    GenerationLimit,
    /// The retained-generation limit is occupied by a predecessor that is
    /// correctly still draining. Refuse a repeated rollover rather than
    /// overwriting it or disguising the wait as a generic capacity failure.
    DrainingCollectionPending,
}

impl fmt::Display for SeamlessRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoGenerationRegistry => f.write_str(
                "no generation registry exists, so there is no successor to hand authority to",
            ),
            Self::RegistrySchemaUnsupported => {
                f.write_str("the generation registry is not the schema this build writes")
            }
            Self::RegistryUnreadable(detail) => {
                write!(f, "the generation registry cannot be trusted: {detail}")
            }
            Self::NoLiveRegisteredActive => {
                f.write_str("no live registered active generation exists")
            }
            Self::GenerationLimit => f.write_str("the generation limit is already reached"),
            Self::DrainingCollectionPending => f.write_str(
                "the generation limit is already reached while a draining generation is still awaiting collection",
            ),
        }
    }
}

/// Why this build cannot hand authority to a live successor right now.
///
/// The answer comes from the durable registry plus exact liveness observation.
/// A free generation slot is required because the synthesis root stages the
/// successor after this preflight.
#[must_use]
pub fn seamless_refusal(
    registry: Option<&RegistryDocument>,
    active_is_alive: bool,
    generation_limit: usize,
) -> Option<SeamlessRefusal> {
    let Some(document) = registry else {
        return Some(SeamlessRefusal::NoGenerationRegistry);
    };
    if document.schema != REGISTRY_SCHEMA {
        return Some(SeamlessRefusal::RegistrySchemaUnsupported);
    }
    if !active_is_alive || document.active().is_none() {
        return Some(SeamlessRefusal::NoLiveRegisteredActive);
    }
    if document.retained() < generation_limit {
        return None;
    }
    Some(
        if document
            .generations
            .iter()
            .any(|entry| entry.role == crate::usecase::generation::GenerationRole::Draining)
        {
            SeamlessRefusal::DrainingCollectionPending
        } else {
            SeamlessRefusal::GenerationLimit
        },
    )
}

/// Whether the operator gave up the live runtime a transition would destroy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionMode {
    /// The ordinary request. It never destroys live runtime.
    Planned,
    /// An explicit cold transition: the caller accepts that every PTY the old
    /// daemon owns is terminated with it.
    Cold,
}

/// What a planned replacement is allowed to do.
#[derive(Debug, PartialEq, Eq)]
pub enum ReplacementPlan {
    /// Stop the recorded daemon and start a fresh one. Whatever it owned is
    /// gone with it.
    ColdTransition,
    /// Stage a standby and ask the old active to drive its own gated handoff.
    SeamlessRollover,
    /// Do nothing at all.
    Refused {
        /// Why the live runtime could not have been preserved instead.
        seamless: SeamlessRefusal,
        /// What a cold transition would have destroyed.
        live: LiveResources,
    },
}

/// Decide a planned replacement from what was observed.
#[must_use]
pub fn plan_replacement(
    mode: TransitionMode,
    seamless: Option<&SeamlessRefusal>,
    live: LiveResources,
) -> ReplacementPlan {
    if live.is_empty() || mode == TransitionMode::Cold {
        ReplacementPlan::ColdTransition
    } else {
        match seamless {
            None => ReplacementPlan::SeamlessRollover,
            Some(seamless) => ReplacementPlan::Refused {
                seamless: seamless.clone(),
                live,
            },
        }
    }
}

/// The synthesis-root operation that stages a standby and sends the rollover
/// IPC verb to the old active daemon.
pub trait RolloverRequester {
    /// Execute the operation and return the user-facing report.
    ///
    /// # Errors
    /// Returns staging, readiness, IPC, or handoff failures.
    fn rollover(&self, operation: &OperationId) -> io::Result<String>;
}

/// What a stop is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopPlan {
    /// Terminate the recorded daemon and reclaim its record.
    Terminate,
    /// Do nothing at all.
    Refused(LiveResources),
}

/// Decide a stop from what was observed.
///
/// Stopping is not a rollover: there is no successor and therefore nothing to
/// hand the live runtime to. The only question is whether the caller gave it up.
#[must_use]
pub const fn plan_stop(mode: TransitionMode, live: LiveResources) -> StopPlan {
    if live.is_empty() || matches!(mode, TransitionMode::Cold) {
        StopPlan::Terminate
    } else {
        StopPlan::Refused(live)
    }
}

/// The durable operation a manual `daemon restart` is attributed to.
///
/// A manual restart is a forced replacement of the artifact that is already
/// running, so it is keyed exactly like the trigger `usagi daemon replace`
/// derives for that case: both verbs attribute their transition to the same
/// durable operation instead of minting an identity each time. It is an
/// attribution key, not a deduplication key — a second deliberate restart is a
/// second transition, and converging an *in-flight* handoff onto one operation
/// is the registry's job ([`super::authority::handoff`]). An unknown artifact
/// identity has no safe key and yields `None`.
#[must_use]
pub fn manual_operation_id(build: &BuildIdentity, channel: &str) -> Option<OperationId> {
    build_rollover_trigger(build, build, channel, true).map(|trigger| trigger.operation_id)
}

/// Refuse a transition that would destroy live runtime.
fn refuse_live(action: &str, live: LiveResources, why: Option<&SeamlessRefusal>) -> io::Error {
    let reason = why.map_or_else(String::new, |refusal| format!("; {refusal}"));
    io::Error::new(
        io::ErrorKind::WouldBlock,
        format!(
            "refusing to {action}: the daemon still owns {live}{reason}. \
             Close them, or ask for an explicit cold transition with --force"
        ),
    )
}

/// What the recorded daemon owns right now, or nothing when no daemon owns it.
///
/// Only a daemon whose exact owner identity is alive can still hold a PTY
/// master. Records a crashed daemon left behind name no owner a transition
/// could take anything from, so a census is not even taken for them — otherwise
/// `daemon stop` would refuse to stop a daemon that is not running.
///
/// # Errors
/// Returns the store's load error or the census error. Neither is treated as
/// "nothing is live".
fn owned_runtime<F: RecordFile, P: LivenessProbe>(
    store: &DaemonRecordStore<F>,
    probe: &P,
    census: &dyn ResourceCensus,
) -> io::Result<LiveResources> {
    let record = store.load()?;
    let observation = record.as_ref().map_or(
        usagi_core::domain::daemon::DaemonProcessObservation::Unknown,
        |record| probe.observe(record),
    );
    if usagi_core::domain::daemon::classify(record.as_ref(), observation) == DaemonState::Alive {
        census.live()
    } else {
        Ok(LiveResources::default())
    }
}

/// Stop the recorded daemon, refusing while it still owns live runtime.
///
/// # Errors
/// Returns the census error, the refusal when live runtime would be destroyed,
/// or the stop phase's error.
// One seam per real-IO concern the stop needs (record store, owner probe,
// terminator, sleeper, stale cleanup, census) plus the mode and app info;
// grouping them would only hide the composition wiring.
#[allow(clippy::too_many_arguments)]
pub fn stop_daemon<F: RecordFile, P: LivenessProbe, T: Terminator, K: Sleeper>(
    store: &DaemonRecordStore<F>,
    probe: &P,
    terminator: &T,
    sleeper: &K,
    stale_cleanup: &dyn StaleDaemonCleanup,
    census: &dyn ResourceCensus,
    mode: TransitionMode,
    info: &AppInfo,
) -> io::Result<String> {
    match plan_stop(mode, owned_runtime(store, probe, census)?) {
        StopPlan::Terminate => stop::stop(store, probe, terminator, sleeper, stale_cleanup, info),
        StopPlan::Refused(live) => Err(refuse_live("stop the daemon", live, None)),
    }
}

/// Replace the recorded daemon with a fresh one under `operation`.
///
/// Both `usagi daemon restart` and the build/update replacement trigger enter
/// here, so neither can reach `stop` → fresh `start` without this guard.
///
/// # Errors
/// Returns the census error, the refusal when live runtime would be destroyed,
/// or the cold transition's stop / start error.
// One seam per real-IO concern the two phases need (record store, owner probe,
// terminator, launcher, sleeper, stale cleanup, census) plus the operation
// identity and app info; grouping them would only hide the composition wiring.
#[allow(clippy::too_many_arguments)]
pub fn replace_daemon<
    F: RecordFile,
    P: LivenessProbe,
    T: Terminator,
    L: DaemonLauncher,
    K: Sleeper,
>(
    store: &DaemonRecordStore<F>,
    probe: &P,
    terminator: &T,
    launcher: &L,
    sleeper: &K,
    stale_cleanup: &dyn StaleDaemonCleanup,
    census: &dyn ResourceCensus,
    seamless: Option<&SeamlessRefusal>,
    rollover: &dyn RolloverRequester,
    mode: TransitionMode,
    operation: Option<&OperationId>,
    info: &AppInfo,
) -> io::Result<String> {
    let live = owned_runtime(store, probe, census)?;
    match plan_replacement(mode, seamless, live) {
        ReplacementPlan::ColdTransition => {
            let report = restart::restart(
                store,
                probe,
                terminator,
                launcher,
                sleeper,
                stale_cleanup,
                info,
            )?;
            Ok(match operation {
                Some(operation) => format!("{report} (operation {})", operation.0),
                None => report,
            })
        }
        ReplacementPlan::SeamlessRollover => {
            let operation = operation.ok_or_else(|| {
                io::Error::other(
                    "daemon artifact identity is unavailable for a durable rollover operation",
                )
            })?;
            rollover.rollover(operation)
        }
        ReplacementPlan::Refused { seamless, live } => {
            Err(refuse_live("replace the daemon", live, Some(&seamless)))
        }
    }
}

#[cfg(test)]
mod tests;
