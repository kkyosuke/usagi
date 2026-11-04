//! Routing IPC requests while a daemon is temporarily more than one process.
//!
//! A planned restart leaves the old generation alive and *draining*: it still
//! owns the PTYs it spawned, while a new generation becomes `current` and takes
//! every new launch. A client that keeps connecting to "the current endpoint"
//! cannot reach those PTYs at all, and a client that guesses would deliver an
//! old terminal's input to a *different* terminal on the new daemon. This
//! module is the routing layer that makes neither possible.
//!
//! Its whole contract is one table:
//!
//! | request | endpoint |
//! |---|---|
//! | control operations (workspace / session / issue / Agent launch, generic terminal launch) | the active generation |
//! | scope inventory | every trusted generation, merged and deduplicated |
//! | attach / resume / resync / input / resize / detach / kill, addressed by a complete `TerminalRef` | the owner named by `TerminalRef.daemon_generation` |
//! | unknown, retired, or forged generation | typed refusal — never the active endpoint, never a same-named terminal |
//!
//! The shared policy preserves these properties:
//!
//! * **The endpoint set is trusted, not client-supplied.** A caller names a
//!   [`DaemonGeneration`]; the endpoint comes from [`TrustedEndpoints`], which a
//!   [`GenerationDirectory`] builds from records only the daemon writes. There
//!   is no API through which a client passes a socket path.
//! * **A partial answer is not an absence.** [`merge_inventory`] retains which
//!   generations answered and which were unreachable, so presentation can keep
//!   an unreachable owner's last-known terminals until authoritative retirement.
//! * **Generations are independent.** The composition root owns each generation's
//!   connection and output cursors; publishing a new `current` does not discard
//!   a draining subscription.
//! * **Resolving costs no IO.** The trusted set comes from files, so
//!   [`RouteCache`] holds it and re-reads only on a reason — the first
//!   resolution, one that fails, or a caller reporting that the endpoint it
//!   named did not answer for that generation. A request sent on a connection
//!   that is already open resolves nothing.
//!
//! The client advertises
//! [`OWNER_GENERATION_ROUTING_CAPABILITY`](crate::infrastructure::ipc::OWNER_GENERATION_ROUTING_CAPABILITY)
//! so a daemon can refuse to start a rollover that would strand a client which
//! lacks this routing. The contract is documented in
//! [4. IPC](../../../../document/04-ipc.md#owner-generation-routing).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::domain::id::{DaemonGeneration, TerminalRef};
use crate::domain::terminal_launch::{TerminalInventoryEntry, TerminalLaunchScope};
use crate::infrastructure::ipc::{ClientError, DaemonRequest, TerminalAction, TerminalRequest};
use crate::infrastructure::ipc::{ErrorCode, GenerationRole, ProtocolError, SideEffect};

/// One generation a client may address, exactly as the daemon published it.
///
/// `endpoint` is carried rather than derived: it is the daemon's own spelling,
/// and the resolver refuses a request whose generation is not in the trusted
/// set instead of composing a path of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedEndpoint {
    pub generation: DaemonGeneration,
    pub role: GenerationRole,
    pub endpoint: String,
}

/// Why a trusted endpoint set could not be produced. Both variants are fail
/// closed: a client with no trustworthy directory routes nothing rather than
/// falling back to the endpoint it used last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectoryError {
    /// The daemon-written records could not be read.
    Unreadable(String),
    /// The records contradict themselves — a duplicated generation, more than
    /// one active generation, or a `current` that names no active entry.
    Corrupt(&'static str),
}

impl fmt::Display for DirectoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable(detail) => write!(f, "generation directory is unreadable: {detail}"),
            Self::Corrupt(detail) => write!(f, "generation directory is inconsistent: {detail}"),
        }
    }
}

impl std::error::Error for DirectoryError {}

/// Every generation a client may currently address.
///
/// A retired generation is simply absent: absence from this set is the verified
/// retirement that lets a tab and its link be collected.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustedEndpoints {
    current: Option<DaemonGeneration>,
    entries: Vec<TrustedEndpoint>,
}

impl TrustedEndpoints {
    /// Validate a directory reading into an addressable set.
    ///
    /// Ordering is normalized here — the active generation first, then draining
    /// generations by identity — so a fan-out is deterministic regardless of how
    /// the records happened to be stored.
    ///
    /// # Errors
    /// Returns [`DirectoryError::Corrupt`] for a duplicated generation, more
    /// than one active generation, or a `current` that does not name the single
    /// active entry.
    pub fn build(
        current: Option<DaemonGeneration>,
        mut entries: Vec<TrustedEndpoint>,
    ) -> Result<Self, DirectoryError> {
        entries.sort_by(|left, right| {
            (left.role != GenerationRole::Active, left.generation)
                .cmp(&(right.role != GenerationRole::Active, right.generation))
        });
        let mut seen = BTreeSet::new();
        for entry in &entries {
            if !seen.insert(entry.generation) {
                return Err(DirectoryError::Corrupt("generation is listed twice"));
            }
        }
        let actives: Vec<_> = entries
            .iter()
            .filter(|entry| entry.role == GenerationRole::Active)
            .map(|entry| entry.generation)
            .collect();
        match (actives.as_slice(), current) {
            ([], None) => {}
            ([active], Some(named)) if *active == named => {}
            _ => {
                return Err(DirectoryError::Corrupt(
                    "current does not name the single active generation",
                ));
            }
        }
        Ok(Self { current, entries })
    }

    /// The generation the current locator names.
    #[must_use]
    pub fn active(&self) -> Option<&TrustedEndpoint> {
        self.current
            .and_then(|current| self.owner(current))
            .filter(|entry| entry.role == GenerationRole::Active)
    }

    /// The entry for `generation`, when it is still addressable.
    #[must_use]
    pub fn owner(&self, generation: DaemonGeneration) -> Option<&TrustedEndpoint> {
        self.entries
            .iter()
            .find(|entry| entry.generation == generation)
    }

    /// Every addressable generation, active first.
    #[must_use]
    pub fn all(&self) -> &[TrustedEndpoint] {
        &self.entries
    }

    /// Whether this set names no generation at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The daemon-written records a client resolves endpoints from.
///
/// The port takes no path and returns no path a caller may substitute: the
/// adapter reads the registry and the current locator the daemon owns, and the
/// only thing a caller names is a [`DaemonGeneration`].
pub trait GenerationDirectory {
    /// Read the currently addressable generations.
    ///
    /// # Errors
    /// Returns [`DirectoryError`] when the records cannot be read or do not
    /// describe one consistent authority.
    fn snapshot(&self) -> Result<TrustedEndpoints, DirectoryError>;
}

/// Where one request belongs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteTarget {
    /// Control work, including a new launch: only the active generation may
    /// accept it.
    ActiveControl,
    /// Work addressed by a complete `TerminalRef`: only its owner may accept it.
    Owner(DaemonGeneration),
    /// A scope query: every trusted generation answers, and the answers merge.
    EveryGeneration,
}

/// Why a request could not be routed. Every variant is effect zero and none of
/// them permits a fallback endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutingError {
    /// The typed payload does not describe the action it was sent with, so the
    /// owner cannot be established from it.
    Unroutable,
    /// No generation currently holds `current`.
    NoActiveGeneration,
    /// The named owner is not in the trusted set: it was never registered, it
    /// has been retired, or the reference is forged.
    UnknownGeneration(DaemonGeneration),
    /// The trusted set itself is unusable.
    Directory(DirectoryError),
}

impl From<DirectoryError> for RoutingError {
    fn from(error: DirectoryError) -> Self {
        Self::Directory(error)
    }
}

impl fmt::Display for RoutingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unroutable => f.write_str("terminal request payload does not match its action"),
            Self::NoActiveGeneration => f.write_str("no active daemon generation is published"),
            Self::UnknownGeneration(generation) => {
                write!(f, "daemon generation {generation} is not addressable")
            }
            Self::Directory(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for RoutingError {}

impl RoutingError {
    /// The typed client failure this refusal presents as.
    ///
    /// An unaddressable owner is [`ErrorCode::StaleTarget`] rather than
    /// [`ErrorCode::Unavailable`]: the reference names something that no longer
    /// exists, which is a different answer from "its owner did not respond".
    #[must_use]
    pub fn to_client_error(&self) -> ClientError {
        let code = match self {
            Self::Unroutable | Self::UnknownGeneration(_) => ErrorCode::StaleTarget,
            Self::NoActiveGeneration | Self::Directory(_) => ErrorCode::Unavailable,
        };
        let mut error = ProtocolError::new(code, self.to_string());
        error.side_effect = SideEffect::None;
        error.error_id = "owner-generation-routing".into();
        if let Self::UnknownGeneration(generation) = self {
            // The unaddressable owner goes in `details`, never in
            // `current_daemon_generation`: that field means "the generation
            // serving you now", and a client uses it to detect a rollover.
            error.details = Some(serde_json::json!({ "owner_generation": generation.as_str() }));
        }
        ClientError::Protocol(error)
    }
}

/// The action a typed terminal payload describes.
///
/// Routing reads the payload, so a payload that disagrees with the action it
/// was sent under must not be routed by either one of them.
#[must_use]
pub fn terminal_action_of(request: &TerminalRequest) -> TerminalAction {
    match request {
        TerminalRequest::Launch { .. } => TerminalAction::Launch,
        TerminalRequest::Inventory { .. } => TerminalAction::Inventory,
        TerminalRequest::Attach { .. } => TerminalAction::Attach,
        TerminalRequest::Resume { .. } => TerminalAction::Resume,
        TerminalRequest::Resync { .. } => TerminalAction::Resync,
        TerminalRequest::Input { .. } => TerminalAction::Input,
        TerminalRequest::InputOutcome { .. } => TerminalAction::InputOutcome,
        TerminalRequest::Resize { .. } => TerminalAction::Resize,
        TerminalRequest::Detach { .. } => TerminalAction::Detach,
        TerminalRequest::CompletedInventory { .. } => TerminalAction::CompletedInventory,
        TerminalRequest::Observe { .. } => TerminalAction::Observe,
        TerminalRequest::Dismiss { .. } => TerminalAction::Dismiss,
    }
}

/// Where a typed terminal request belongs.
///
/// A launch is control work — only the active generation spawns — while every
/// request carrying a complete `TerminalRef` follows that reference's owner,
/// including the tombstone visibility swaps, whose compare-and-swap is held by
/// the generation that recorded the tombstone.
#[must_use]
pub fn route_terminal_request(request: &TerminalRequest) -> RouteTarget {
    match request {
        TerminalRequest::Launch { .. } => RouteTarget::ActiveControl,
        TerminalRequest::Inventory { .. } | TerminalRequest::CompletedInventory { .. } => {
            RouteTarget::EveryGeneration
        }
        TerminalRequest::Attach { terminal, .. }
        | TerminalRequest::Resume { terminal, .. }
        | TerminalRequest::Resync { terminal }
        | TerminalRequest::Input { terminal, .. }
        | TerminalRequest::InputOutcome { terminal, .. }
        | TerminalRequest::Resize { terminal, .. }
        | TerminalRequest::Detach { terminal, .. }
        | TerminalRequest::Observe { terminal, .. }
        | TerminalRequest::Dismiss { terminal, .. } => {
            RouteTarget::Owner(terminal.daemon_generation)
        }
    }
}

/// Where a whole daemon request belongs.
///
/// Everything that is not a terminal request is control work on the active
/// generation. A terminal request is classified from its **typed** payload, and
/// a payload that does not decode, or decodes to a different action than the
/// one it was sent under, is refused rather than routed by its action alone.
///
/// # Errors
/// Returns [`RoutingError::Unroutable`] for a terminal payload that cannot be
/// trusted to name its own owner.
pub fn route_daemon_request(request: &DaemonRequest) -> Result<RouteTarget, RoutingError> {
    let DaemonRequest::Terminal { action, payload } = request else {
        return Ok(RouteTarget::ActiveControl);
    };
    let terminal: TerminalRequest =
        serde_json::from_value(payload.clone()).map_err(|_| RoutingError::Unroutable)?;
    if terminal_action_of(&terminal) != *action {
        return Err(RoutingError::Unroutable);
    }
    Ok(route_terminal_request(&terminal))
}

/// The endpoints a routed request is sent to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteResolution {
    /// Exactly one endpoint answers.
    Single(TrustedEndpoint),
    /// Every trusted endpoint answers and the answers merge.
    FanOut(Vec<TrustedEndpoint>),
}

impl RouteResolution {
    /// Every endpoint this resolution names.
    #[must_use]
    pub fn into_endpoints(self) -> Vec<TrustedEndpoint> {
        match self {
            Self::Single(endpoint) => vec![endpoint],
            Self::FanOut(endpoints) => endpoints,
        }
    }
}

/// Resolve a target against the trusted set.
///
/// # Errors
/// Returns [`RoutingError::NoActiveGeneration`] when nothing holds `current`,
/// or [`RoutingError::UnknownGeneration`] when the named owner is not
/// addressable. Neither ever degrades into the active endpoint.
pub fn resolve_route(
    target: &RouteTarget,
    endpoints: &TrustedEndpoints,
) -> Result<RouteResolution, RoutingError> {
    match target {
        RouteTarget::ActiveControl => endpoints
            .active()
            .cloned()
            .map(RouteResolution::Single)
            .ok_or(RoutingError::NoActiveGeneration),
        RouteTarget::Owner(generation) => endpoints
            .owner(*generation)
            .cloned()
            .map(RouteResolution::Single)
            .ok_or(RoutingError::UnknownGeneration(*generation)),
        RouteTarget::EveryGeneration => {
            if endpoints.is_empty() {
                return Err(RoutingError::NoActiveGeneration);
            }
            Ok(RouteResolution::FanOut(endpoints.all().to_vec()))
        }
    }
}

/// What one generation answered for a scope inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventoryOutcome {
    /// The generation answered authoritatively for the requested scope.
    Listed(Vec<TerminalInventoryEntry>),
    /// The generation did not answer. This is uncertainty, never an absence.
    Unreachable,
}

/// One generation's contribution to a merged inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationInventory {
    pub generation: DaemonGeneration,
    pub outcome: InventoryOutcome,
}

/// A scope inventory assembled from every trusted generation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergedInventory {
    entries: Vec<TerminalInventoryEntry>,
    answered: BTreeSet<DaemonGeneration>,
    unreachable: BTreeSet<DaemonGeneration>,
}

impl MergedInventory {
    /// The merged runtimes, deduplicated by exact reference and in a
    /// deterministic order.
    #[must_use]
    pub fn entries(&self) -> &[TerminalInventoryEntry] {
        &self.entries
    }

    /// Generations that answered authoritatively.
    #[must_use]
    pub fn answered(&self) -> &BTreeSet<DaemonGeneration> {
        &self.answered
    }

    /// Generations whose answer is missing. Their terminals stay reconnecting.
    #[must_use]
    pub fn unreachable(&self) -> &BTreeSet<DaemonGeneration> {
        &self.unreachable
    }

    /// Whether at least one generation could not be reached.
    #[must_use]
    pub fn is_partial(&self) -> bool {
        !self.unreachable.is_empty()
    }

    /// Whether any generation answered at all. A merge in which nothing
    /// answered says nothing about any terminal.
    #[must_use]
    pub fn answered_any(&self) -> bool {
        !self.answered.is_empty()
    }
}

/// Merge every generation's answer for one scope.
///
/// Two fences run before an entry is accepted, because a generation may only
/// speak for its own resources in the scope that was asked for:
///
/// * an entry whose `daemon_generation` is not the answering generation is
///   dropped — one daemon cannot introduce another's terminal;
/// * an entry outside the requested scope is dropped.
///
/// What remains is deduplicated by exact [`TerminalRef`] and ordered by it, so
/// repeating the merge over the same answers projects each terminal onto
/// exactly one tab, in the same order every time.
#[must_use]
pub fn merge_inventory(
    parts: &[GenerationInventory],
    scope: &TerminalLaunchScope,
) -> MergedInventory {
    let mut merged: BTreeMap<TerminalRef, TerminalInventoryEntry> = BTreeMap::new();
    let mut answered = BTreeSet::new();
    let mut unreachable = BTreeSet::new();
    for part in parts {
        let InventoryOutcome::Listed(entries) = &part.outcome else {
            unreachable.insert(part.generation);
            continue;
        };
        answered.insert(part.generation);
        for entry in entries {
            if entry.terminal.daemon_generation == part.generation
                && entry.terminal.workspace_id == scope.workspace_id
                && entry.terminal.session_id == scope.session_id
                && entry.terminal.worktree_id == scope.worktree_id
            {
                merged.insert(entry.terminal.clone(), entry.clone());
            }
        }
    }
    MergedInventory {
        entries: merged.into_values().collect(),
        answered,
        unreachable,
    }
}

/// The trusted snapshot a client resolves against, read from the directory only
/// when there is a reason to read it.
///
/// The registry and the current locator are files. Reading them once per request
/// would put a directory traversal and two `open`/`read` pairs on the IPC hot
/// path, which is the cost that had to be taken back out of the daemon's own PTY
/// path (#555). So the snapshot is cached and re-read only on a reason:
///
/// | trigger | why |
/// |---|---|
/// | the first resolution | there is nothing to resolve against yet |
/// | a resolution that fails | the snapshot may predate the handoff that published this owner |
/// | [`RouteCache::invalidate`] | the endpoint it named could not be reached, or the peer there named a different generation |
///
/// A second failure after the refresh is the answer: nothing degrades into the
/// active endpoint.
pub struct RouteCache {
    directory: Box<dyn GenerationDirectory + Send>,
    endpoints: TrustedEndpoints,
    loaded: bool,
}

impl RouteCache {
    /// Build a cache over a trusted directory. Nothing is read until the first
    /// resolution, so constructing one costs no IO.
    ///
    /// The directory is `Send` because one client process resolves owners from
    /// its render thread and from its background observation lanes, behind one
    /// shared cache rather than one directory reader per thread.
    pub fn new(directory: impl GenerationDirectory + Send + 'static) -> Self {
        Self {
            directory: Box::new(directory),
            endpoints: TrustedEndpoints::default(),
            loaded: false,
        }
    }

    /// The trusted set this cache last read.
    #[must_use]
    pub fn endpoints(&self) -> &TrustedEndpoints {
        &self.endpoints
    }

    /// Whether the directory has been read at least once.
    #[must_use]
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// Re-read the trusted directory.
    ///
    /// # Errors
    /// Returns [`DirectoryError`] and leaves the previous snapshot in place, so
    /// an unreadable directory does not silently unaddress a live owner.
    pub fn refresh(&mut self) -> Result<(), DirectoryError> {
        self.endpoints = self.directory.snapshot()?;
        self.loaded = true;
        Ok(())
    }

    /// Require the next resolution to re-read the directory.
    ///
    /// This is the caller's way of reporting evidence the cache cannot see for
    /// itself: the endpoint this snapshot named could not be reached, or the
    /// peer at it named a generation other than the one that was asked for.
    /// Both mean the records may have moved on, and neither is visible from the
    /// snapshot.
    pub fn invalidate(&mut self) {
        self.loaded = false;
    }

    /// Resolve `target`, refreshing once when the current snapshot cannot.
    ///
    /// # Errors
    /// Returns the [`RoutingError`] the *first* attempt produced, so a refresh
    /// that changes nothing does not relabel an unaddressable owner. A directory
    /// that cannot be read at all is [`RoutingError::Directory`].
    pub fn resolve(&mut self, target: &RouteTarget) -> Result<RouteResolution, RoutingError> {
        if !self.loaded {
            self.refresh()?;
        }
        match resolve_route(target, &self.endpoints) {
            Ok(resolution) => Ok(resolution),
            Err(first) => {
                // The snapshot may predate a handoff that published this owner.
                // One refresh is enough: a second failure is the answer.
                self.refresh()?;
                resolve_route(target, &self.endpoints).map_err(|_| first)
            }
        }
    }

    /// Resolve the single endpoint that owns `generation`.
    ///
    /// # Errors
    /// Returns [`RoutingError::UnknownGeneration`] when the generation is not
    /// addressable — never the active endpoint.
    pub fn owner(&mut self, generation: DaemonGeneration) -> Result<TrustedEndpoint, RoutingError> {
        self.resolve(&RouteTarget::Owner(generation))?
            .into_endpoints()
            .pop()
            .ok_or(RoutingError::UnknownGeneration(generation))
    }

    /// Every generation a scope inventory must be asked, active first.
    ///
    /// # Errors
    /// Returns [`RoutingError::NoActiveGeneration`] when there is no generation
    /// to ask at all.
    pub fn every_generation(&mut self) -> Result<Vec<TrustedEndpoint>, RoutingError> {
        Ok(self
            .resolve(&RouteTarget::EveryGeneration)?
            .into_endpoints())
    }
}

/// Wrap a typed terminal request in the daemon request that carries it.
///
/// The action is derived from the payload, so the two cannot disagree on a
/// request this client sends.
///
/// # Panics
/// Panics only if [`TerminalRequest`] stops being serializable, which its own
/// derive guarantees it cannot.
#[must_use]
pub fn terminal_request(request: &TerminalRequest) -> DaemonRequest {
    DaemonRequest::Terminal {
        action: terminal_action_of(request),
        payload: serde_json::to_value(request).expect("terminal request is serializable"),
    }
}

#[cfg(test)]
mod tests;
