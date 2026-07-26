//! Addressing a daemon that is temporarily more than one process.
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
//! Three properties are load bearing, and each has a dedicated piece here:
//!
//! * **The endpoint set is trusted, not client-supplied.** A caller names a
//!   [`DaemonGeneration`]; the endpoint comes from [`TrustedEndpoints`], which a
//!   [`GenerationDirectory`] builds from records only the daemon writes. There
//!   is no API through which a client passes a socket path.
//! * **A partial answer is not an absence.** A draining owner that times out
//!   leaves its terminals [`OwnerPresence::Reconnecting`]; only an authoritative
//!   non-live answer or a verified retirement collects a tab
//!   ([`merge_inventory`], [`presence_of`]).
//! * **Generations are independent.** Connections and output cursors live in
//!   [`GenerationLinks`], keyed by generation, so publishing a new `current`
//!   does not discard a draining subscription.
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
use crate::infrastructure::ipc::{ErrorCode, GenerationRole, ProtocolError, SideEffect};
use crate::usecase::client::{
    ClientError, DaemonReply, DaemonRequest, DaemonSession, TerminalAction, TerminalRequest,
};

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

/// Establishes one generation's connection on demand, as a callback the link
/// set can hold without knowing the transport.
pub type Connect<'a> =
    &'a mut dyn FnMut(&TrustedEndpoint) -> Result<Box<dyn DaemonSession>, ClientError>;

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
        TerminalRequest::Attach { terminal }
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

/// What a merged inventory says about one tracked terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerPresence {
    /// Its owner listed it as live.
    Live,
    /// Its owner answered and it is not live, or its generation is verifiably
    /// retired. Only this collects the tab.
    Gone,
    /// Nothing authoritative is known: the owner did not answer, or it was not
    /// even asked. The last-known tab is kept.
    Reconnecting,
}

/// Decide what a merged inventory proves about one tracked terminal.
///
/// The asymmetry is deliberate. Liveness and death both require the owner to
/// have spoken; silence — a timed-out draining endpoint, a partial merge, a
/// generation that was not part of this round — never does. A generation that
/// has left the trusted set, on the other hand, is a *verified* retirement: its
/// process is gone by the registry's own authority, so its terminals are gone
/// with it.
#[must_use]
pub fn presence_of(
    terminal: &TerminalRef,
    merged: &MergedInventory,
    endpoints: &TrustedEndpoints,
) -> OwnerPresence {
    let generation = terminal.daemon_generation;
    if endpoints.owner(generation).is_none() {
        return OwnerPresence::Gone;
    }
    if let Some(entry) = merged
        .entries
        .iter()
        .find(|entry| &entry.terminal == terminal)
    {
        return if entry.live {
            OwnerPresence::Live
        } else {
            OwnerPresence::Gone
        };
    }
    if merged.answered.contains(&generation) {
        OwnerPresence::Gone
    } else {
        OwnerPresence::Reconnecting
    }
}

/// One generation's client-side link: its connection and its output cursors.
struct GenerationLink {
    endpoint: String,
    session: Option<Box<dyn DaemonSession>>,
    cursors: BTreeMap<TerminalRef, u64>,
}

/// Per-generation connections and output cursors.
///
/// Keying by generation is what makes a rollover survivable on the client side:
/// publishing a new `current` adds a link, and the draining link — its socket,
/// its negotiated capabilities, and its cursors — is untouched. A transport
/// failure drops only that generation's socket and *keeps* its cursors, so the
/// reconnect resumes from the last applied offset instead of replaying output
/// the tab already has.
/// The session type is erased on purpose: one router holds connections of the
/// same kind to different generations, so nothing here needs to be generic over
/// it, and dynamic dispatch on a per-request path costs nothing measurable.
#[derive(Default)]
pub struct GenerationLinks {
    links: BTreeMap<DaemonGeneration, GenerationLink>,
}

impl GenerationLinks {
    /// An empty set of links.
    #[must_use]
    pub fn new() -> Self {
        Self {
            links: BTreeMap::new(),
        }
    }

    /// How many generations are linked.
    #[must_use]
    pub fn len(&self) -> usize {
        self.links.len()
    }

    /// Whether no generation is linked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
    }

    /// Whether a live connection is currently held for `generation`.
    #[must_use]
    pub fn is_connected(&self, generation: DaemonGeneration) -> bool {
        self.links
            .get(&generation)
            .is_some_and(|link| link.session.is_some())
    }

    /// The connection for `endpoint`, establishing one only when there is none.
    ///
    /// A generation whose published endpoint changed is treated as a new link:
    /// the old socket is dropped rather than reused for an endpoint the daemon
    /// no longer names.
    ///
    /// # Errors
    /// Returns `connect`'s error, leaving the link without a session.
    pub fn session(
        &mut self,
        endpoint: &TrustedEndpoint,
        connect: Connect<'_>,
    ) -> Result<&mut dyn DaemonSession, ClientError> {
        let link = self
            .links
            .entry(endpoint.generation)
            .or_insert_with(|| GenerationLink {
                endpoint: endpoint.endpoint.clone(),
                session: None,
                cursors: BTreeMap::new(),
            });
        if link.endpoint != endpoint.endpoint {
            link.endpoint.clone_from(&endpoint.endpoint);
            link.session = None;
        }
        match &mut link.session {
            Some(session) => Ok(session.as_mut()),
            slot @ None => Ok(slot.insert(connect(endpoint)?).as_mut()),
        }
    }

    /// Drop one generation's connection after a transport failure, keeping its
    /// cursors so the reconnect resumes rather than replays.
    pub fn invalidate(&mut self, generation: DaemonGeneration) {
        if let Some(link) = self.links.get_mut(&generation) {
            link.session = None;
        }
    }

    /// Forget every generation the trusted set no longer names.
    ///
    /// Only a verified retirement removes a generation from that set, so this
    /// never collects a link whose owner is merely unreachable.
    pub fn retain_trusted(&mut self, endpoints: &TrustedEndpoints) {
        self.links
            .retain(|generation, _| endpoints.owner(*generation).is_some());
    }

    /// The offset this client has already applied for `terminal`.
    #[must_use]
    pub fn cursor(&self, terminal: &TerminalRef) -> Option<u64> {
        self.links
            .get(&terminal.daemon_generation)
            .and_then(|link| link.cursors.get(terminal).copied())
    }

    /// Record progress for `terminal`, keeping the cursor monotonic.
    ///
    /// Returns whether the cursor advanced. A late frame from a reconnected
    /// stream therefore cannot rewind a tab into replaying output it has
    /// already shown.
    pub fn advance_cursor(&mut self, terminal: &TerminalRef, offset: u64) -> bool {
        let Some(link) = self.links.get_mut(&terminal.daemon_generation) else {
            return false;
        };
        let cursor = link.cursors.entry(terminal.clone()).or_default();
        if offset > *cursor {
            *cursor = offset;
            return true;
        }
        false
    }

    /// Forget one terminal's cursor once its owner reported it gone.
    pub fn forget(&mut self, terminal: &TerminalRef) {
        if let Some(link) = self.links.get_mut(&terminal.daemon_generation) {
            link.cursors.remove(terminal);
        }
    }
}

/// Establishes one connection per generation on behalf of the router.
///
/// The implementation is given a [`TrustedEndpoint`] that came from the
/// directory, never a caller-chosen address.
pub trait GenerationTransport {
    /// Connect to one trusted generation endpoint.
    ///
    /// # Errors
    /// Returns the typed failure that leaves the generation unreachable.
    fn connect(
        &mut self,
        endpoint: &TrustedEndpoint,
    ) -> Result<Box<dyn DaemonSession>, ClientError>;
}

/// The production route: a client that addresses every request by its owner.
///
/// It holds the last trusted snapshot and one link per generation, and refreshes
/// the snapshot only when it has a reason to — the first request, an owner it
/// cannot resolve, or a transport failure. A request whose owner is still not
/// addressable after a refresh is refused; nothing is retried against a
/// different endpoint.
pub struct OwnerRouter {
    directory: Box<dyn GenerationDirectory>,
    transport: Box<dyn GenerationTransport>,
    endpoints: TrustedEndpoints,
    loaded: bool,
    links: GenerationLinks,
}

impl OwnerRouter {
    /// Build a router over a trusted directory and a transport.
    pub fn new(
        directory: impl GenerationDirectory + 'static,
        transport: impl GenerationTransport + 'static,
    ) -> Self {
        Self {
            directory: Box::new(directory),
            transport: Box::new(transport),
            endpoints: TrustedEndpoints::default(),
            loaded: false,
            links: GenerationLinks::new(),
        }
    }

    /// The trusted set this router last read.
    #[must_use]
    pub fn endpoints(&self) -> &TrustedEndpoints {
        &self.endpoints
    }

    /// The per-generation links, for cursor bookkeeping.
    #[must_use]
    pub fn links(&self) -> &GenerationLinks {
        &self.links
    }

    /// Mutable access to the per-generation links.
    pub fn links_mut(&mut self) -> &mut GenerationLinks {
        &mut self.links
    }

    /// Re-read the trusted directory and collect links for retired generations.
    ///
    /// # Errors
    /// Returns [`DirectoryError`] and leaves the previous snapshot in place, so
    /// an unreadable directory does not silently unaddress a live owner.
    pub fn refresh(&mut self) -> Result<(), DirectoryError> {
        let endpoints = self.directory.snapshot()?;
        self.links.retain_trusted(&endpoints);
        self.endpoints = endpoints;
        self.loaded = true;
        Ok(())
    }

    /// Resolve `target`, refreshing once when the current snapshot cannot.
    fn resolve(&mut self, target: &RouteTarget) -> Result<RouteResolution, RoutingError> {
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

    /// Send one request to the endpoint that owns it.
    ///
    /// Scope inventory is not routed here — it has more than one answer, so it
    /// is [`OwnerRouter::inventory`].
    ///
    /// # Errors
    /// Returns the routing refusal, the connect failure, or the daemon's own
    /// typed error. A transport failure drops that generation's connection and
    /// is reported as-is: this layer never replays a request on another
    /// endpoint.
    pub fn request(&mut self, request: DaemonRequest) -> Result<DaemonReply, ClientError> {
        let target = route_daemon_request(&request).map_err(|error| error.to_client_error())?;
        let resolution = self
            .resolve(&target)
            .map_err(|error| error.to_client_error())?;
        match resolution {
            RouteResolution::Single(endpoint) => self.exchange(&endpoint, request),
            // A scope query has more than one answer, so sending it here would
            // silently return one generation's view as if it were the whole one.
            RouteResolution::FanOut(_) => Err(RoutingError::Unroutable.to_client_error()),
        }
    }

    /// Ask every trusted generation for one scope and merge the answers.
    ///
    /// A generation that fails to answer is recorded as unreachable and its
    /// link is dropped; the merge still returns, because a missing answer is
    /// uncertainty about that generation only.
    ///
    /// # Errors
    /// Returns the routing refusal when there is no generation to ask at all.
    pub fn inventory(
        &mut self,
        scope: &TerminalLaunchScope,
    ) -> Result<MergedInventory, ClientError> {
        let endpoints = self
            .resolve(&RouteTarget::EveryGeneration)
            .map_err(|error| error.to_client_error())?
            .into_endpoints();
        let request = TerminalRequest::Inventory {
            scope: scope.clone(),
        };
        let mut parts = Vec::with_capacity(endpoints.len());
        for endpoint in endpoints {
            let outcome = match self.exchange(&endpoint, terminal_request(&request)) {
                Ok(reply) => decode_inventory(&reply)
                    .map_or(InventoryOutcome::Unreachable, |entries| {
                        InventoryOutcome::Listed(entries)
                    }),
                Err(_) => InventoryOutcome::Unreachable,
            };
            parts.push(GenerationInventory {
                generation: endpoint.generation,
                outcome,
            });
        }
        Ok(merge_inventory(&parts, scope))
    }

    /// One exchange on one generation's own connection.
    fn exchange(
        &mut self,
        endpoint: &TrustedEndpoint,
        request: DaemonRequest,
    ) -> Result<DaemonReply, ClientError> {
        let transport = &mut self.transport;
        let session = self
            .links
            .session(endpoint, &mut |target| transport.connect(target))?;
        match session.exchange(request) {
            Ok(reply) => Ok(reply),
            Err(error) => {
                if error.is_transport_failure() {
                    self.links.invalidate(endpoint.generation);
                }
                Err(error)
            }
        }
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

/// Decode a scope inventory reply.
fn decode_inventory(reply: &DaemonReply) -> Option<Vec<TerminalInventoryEntry>> {
    let (DaemonReply::Ok(body) | DaemonReply::Accepted { body, .. }) = reply;
    body.get("terminals")?
        .as_array()?
        .iter()
        .map(|item| serde_json::from_value(item.clone()).ok())
        .collect()
}

#[cfg(test)]
mod tests;
