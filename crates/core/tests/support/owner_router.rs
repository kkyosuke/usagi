//! Test-only client harness for the shared owner-routing policy.
//!
//! Core unit tests use fake endpoints and daemon integration tests use real
//! sockets. Production owns generation lanes in the composition root instead;
//! this harness keeps those test scenarios off the public runtime API.

use std::collections::BTreeMap;

use super::core_crate::domain::id::{DaemonGeneration, TerminalRef};
use super::core_crate::domain::terminal_launch::{TerminalInventoryEntry, TerminalLaunchScope};
use super::core_crate::infrastructure::client::DaemonSession;
use super::core_crate::infrastructure::ipc::{
    ClientError, DaemonReply, DaemonRequest, TerminalRequest,
};
use super::core_crate::infrastructure::owner_routing::*;

/// Establishes one generation's connection on demand, as a callback the link
/// set can hold without knowing the transport.
pub type Connect<'a> =
    &'a mut dyn FnMut(&TrustedEndpoint) -> Result<Box<dyn DaemonSession>, ClientError>;

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
        .entries()
        .iter()
        .find(|entry| &entry.terminal == terminal)
    {
        return if entry.live {
            OwnerPresence::Live
        } else {
            OwnerPresence::Gone
        };
    }
    if merged.answered().contains(&generation) {
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

/// Test client that drives the shared routing policy against injected endpoints.
///
/// It holds the last trusted snapshot ([`RouteCache`]) and one link per
/// generation. A request whose owner is still not addressable after a refresh is
/// refused; nothing is retried against a different endpoint.
pub struct OwnerRouter {
    cache: RouteCache,
    transport: Box<dyn GenerationTransport>,
    links: GenerationLinks,
}

impl OwnerRouter {
    /// Build a router over a trusted directory and a transport.
    pub fn new(
        directory: impl GenerationDirectory + Send + 'static,
        transport: impl GenerationTransport + 'static,
    ) -> Self {
        Self {
            cache: RouteCache::new(directory),
            transport: Box::new(transport),
            links: GenerationLinks::new(),
        }
    }

    /// The trusted set this router last read.
    #[must_use]
    pub fn endpoints(&self) -> &TrustedEndpoints {
        self.cache.endpoints()
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
        self.cache.refresh()?;
        self.links.retain_trusted(self.cache.endpoints());
        Ok(())
    }

    /// Resolve `target`, refreshing once when the current snapshot cannot.
    ///
    /// Links are pruned against whatever snapshot the cache ends up holding.
    /// Pruning is idempotent, so doing it after every resolution costs nothing
    /// when the snapshot did not change and cannot miss a refresh that happened
    /// inside the cache.
    fn resolve(&mut self, target: &RouteTarget) -> Result<RouteResolution, RoutingError> {
        let resolution = self.cache.resolve(target);
        self.links.retain_trusted(self.cache.endpoints());
        resolution
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

/// Decode a scope inventory reply.
fn decode_inventory(reply: &DaemonReply) -> Option<Vec<TerminalInventoryEntry>> {
    let (DaemonReply::Ok(body) | DaemonReply::Accepted { body, .. }) = reply;
    body.get("terminals")?
        .as_array()?
        .iter()
        .map(|item| serde_json::from_value(item.clone()).ok())
        .collect()
}
