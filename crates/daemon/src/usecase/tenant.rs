//! The workspaces one daemon owns at the same time.
//!
//! A daemon is one process per machine, but its authority is per *workspace*:
//! the git worktrees, `usagi/<name>` branches, and session names under
//! `<workspace>/.usagi`. Owning several workspaces therefore means holding
//! several [`WorkspaceFence`]s and several lifecycle documents — it does not
//! mean relaxing either. This registry is where that plurality lives, so the
//! rest of the daemon keeps asking for one workspace at a time.
//!
//! Adoption is what turns a workspace into a tenant: fence it, open its
//! lifecycle document from its own state subtree, and remember both. The fence
//! is held for as long as the tenant is, which is what keeps the invariant
//! "one canonical workspace root has one owner on this machine" true while the
//! owner serves many roots.
//!
//! The **initial** tenant is the exception: `serve` fences the workspace it was
//! started in before anything opens, so adopting it again from here would ask
//! the OS for a lock this very process already holds — and `flock` refuses that
//! across two descriptors. [`TenantRegistry::adopt_initial`] registers it with
//! the fence that is already held.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

use chrono::{DateTime, Utc};
use usagi_core::domain::id::WorkspaceId;
use usagi_core::infrastructure::daemon::{WorkspaceFence, WorkspaceFenceOutcome};
use usagi_core::infrastructure::workspace_state;

/// The lifecycle runtime of one workspace, shared between the connections and
/// workers that serve it.
pub type SharedSessionRuntime =
    std::sync::Arc<Mutex<crate::usecase::session_runtime::SessionRuntime>>;

/// Read access to the workspaces a daemon holds, for the components that are
/// daemon-wide but act on one workspace at a time.
///
/// The PTY registry, the Agent runtime, its provisioners, and the teardown
/// worker are single objects for the whole daemon — they own process-level
/// resources — yet every request they serve names a workspace. They resolve it
/// through this port instead of capturing one workspace's runtime at
/// construction, which is what let a daemon serve exactly one workspace.
pub trait WorkspaceRuntimes: Send + Sync {
    /// The workspace with this durable identity, when this daemon holds it.
    fn workspace(&self, workspace: WorkspaceId) -> Option<Tenant<SharedSessionRuntime>>;

    /// The workspace adopted for exactly this root.
    fn workspace_at(&self, root: &Path) -> Option<Tenant<SharedSessionRuntime>>;

    /// The workspace that owns this path: the one whose root is the path itself
    /// or its closest adopted ancestor.
    fn owner_of_path(&self, path: &Path) -> Option<Tenant<SharedSessionRuntime>>;

    /// Every workspace this daemon holds, ordered by root.
    fn all(&self) -> Vec<Tenant<SharedSessionRuntime>>;
}

/// How many workspaces one daemon may hold at once.
///
/// The bound is not about memory: every tenant holds a workspace fence, so an
/// unbounded registry would let one long-lived daemon accumulate ownership of
/// every workspace a user ever opened and never give any of them back.
pub const DEFAULT_TENANT_LIMIT: usize = 32;

/// A workspace lifecycle runtime, opened for one tenant.
pub struct OpenedTenant<R> {
    /// The lifecycle runtime the rest of the daemon talks to.
    pub runtime: R,
    /// The durable workspace identity the runtime bound.
    pub workspace_id: WorkspaceId,
}

/// Opens the lifecycle runtime of one workspace.
///
/// Real git and filesystem IO is bound at the synthesis root; the registry only
/// decides *which* workspace is opened and *where* its document lives.
pub trait TenantRuntimeOpener {
    /// The runtime handle the daemon shares between connections.
    type Runtime: Clone;

    /// Open `workspace_root`, reading and writing its lifecycle document in
    /// `state_dir`.
    ///
    /// # Errors
    ///
    /// Returns an error when the document cannot be opened or reconciled.
    fn open(
        &self,
        workspace_root: &Path,
        state_dir: &Path,
    ) -> io::Result<OpenedTenant<Self::Runtime>>;
}

/// Creates the workspace fence guarding one workspace root.
pub trait WorkspaceFenceFactory {
    /// The fence for `workspace_root`. Acquisition is the caller's next step;
    /// holding the returned value is what keeps the workspace owned.
    fn fence_for(&self, workspace_root: &Path) -> Box<dyn WorkspaceFence + Send>;
}

/// Why a workspace could not be adopted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdoptError {
    /// Another daemon owns the workspace. This refuses that workspace only;
    /// the tenants this daemon already holds are untouched.
    Owned {
        /// The canonical workspace root that is already owned.
        workspace: String,
        /// The owning daemon's pid, when its hint is readable.
        owner: Option<u32>,
    },
    /// This daemon already holds as many workspaces as it may.
    LimitReached {
        /// The bound that was reached.
        limit: usize,
    },
    /// The state subtree or the lifecycle document could not be opened. The
    /// message is safe to show: it names paths, never document contents.
    Storage(String),
}

/// Why an explicit tenant retirement was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetireError {
    /// This daemon does not currently hold the requested root.
    NotFound,
    /// The startup workspace is fenced by `serve` for the process lifetime.
    Initial,
    /// A connection or worker is currently serving this tenant.
    Busy,
}

impl std::fmt::Display for AdoptError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Owned {
                workspace,
                owner: Some(owner),
            } => write!(
                formatter,
                "another daemon already owns this workspace ({workspace}, pid {owner})"
            ),
            Self::Owned {
                workspace,
                owner: None,
            } => write!(
                formatter,
                "another daemon already owns this workspace ({workspace})"
            ),
            Self::LimitReached { limit } => write!(
                formatter,
                "this daemon already holds {limit} workspaces; retire one before opening another"
            ),
            Self::Storage(reason) => write!(formatter, "{reason}"),
        }
    }
}

impl std::error::Error for AdoptError {}

impl From<AdoptError> for io::Error {
    /// The composition root speaks `io::Error`, and every adoption failure is
    /// safe to show: the refusals name a workspace and a pid, and the storage
    /// one names paths.
    fn from(error: AdoptError) -> Self {
        Self::other(error.to_string())
    }
}

/// One adopted workspace, as the rest of the daemon sees it.
///
/// The fence is deliberately *not* here: it is not `Sync`, and a tenant handle
/// travels between connection threads. The registry holds the fence instead, so
/// dropping the registry entry — and only that — gives the workspace back.
///
/// The identity is shared rather than copied, so the registry can see whether
/// anything outside it still holds this workspace. That count is what keeps a
/// retirement from taking a workspace a connection is still serving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tenant<R> {
    shared: std::sync::Arc<TenantIdentity<R>>,
}

#[derive(Debug, PartialEq, Eq)]
struct TenantIdentity<R> {
    root: PathBuf,
    state_dir: PathBuf,
    workspace_id: WorkspaceId,
    runtime: R,
}

impl<R> Tenant<R> {
    /// The canonical workspace root this tenant owns.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.shared.root
    }

    /// The state subtree holding this workspace's lifecycle document.
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.shared.state_dir
    }

    /// The durable workspace identity, as requests fence on it.
    #[must_use]
    pub fn workspace_id(&self) -> WorkspaceId {
        self.shared.workspace_id
    }

    /// The lifecycle runtime that serves this workspace.
    #[must_use]
    pub fn runtime(&self) -> &R {
        &self.shared.runtime
    }

    /// Whether anything outside the registry still holds this workspace.
    ///
    /// The registry keeps one handle per adopted workspace; every connection or
    /// worker that resolved it holds another. Read under the registry's lock,
    /// this distinguishes "nobody is serving this workspace" from "somebody
    /// might send the next request on it".
    fn referenced_elsewhere(&self) -> bool {
        std::sync::Arc::strong_count(&self.shared) > 1
    }
}

/// The workspaces this daemon holds, and the fences that keep them.
pub struct TenantRegistry<F, O: TenantRuntimeOpener> {
    daemon_dir: PathBuf,
    fences: F,
    opener: O,
    limit: usize,
    /// Roots currently inside the miss → fence → register transaction. Equal
    /// roots wait for one another; unrelated workspace adoptions remain
    /// independent.
    adopting: Mutex<BTreeSet<PathBuf>>,
    adoption_changed: Condvar,
    held: Mutex<BTreeMap<PathBuf, Held<O::Runtime>>>,
}

/// Releases one root's adoption lane on every return path, including panic.
struct AdoptionPermit<'a> {
    root: PathBuf,
    adopting: &'a Mutex<BTreeSet<PathBuf>>,
    changed: &'a Condvar,
}

impl Drop for AdoptionPermit<'_> {
    fn drop(&mut self) {
        let mut adopting = self
            .adopting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        adopting.remove(&self.root);
        drop(adopting);
        self.changed.notify_all();
    }
}

/// Observes whether a workspace still has work only its owner can serve.
///
/// Retirement asks this before giving a workspace back: a live PTY child, an
/// Agent runtime, a teardown still to run, or a session mid-creation all mean
/// the daemon must keep the workspace, however long the client has been away.
pub trait WorkspaceActivity<R> {
    /// Whether the workspace with this identity still has work of its own.
    ///
    /// An observation that cannot be made answers `true`: keeping a workspace
    /// costs a fence, while releasing one that is still working would hand its
    /// worktrees to a second owner.
    ///
    /// The identity and the runtime are passed separately, rather than the
    /// tenant handle, so an observation cannot hold a reference to the tenant it
    /// is reporting on — that reference is itself a reason to keep the
    /// workspace.
    fn has_work(&self, workspace: WorkspaceId, runtime: &R) -> bool;
}

/// A tenant together with the fence that keeps it.
///
/// The fence is never read after it is stored: holding it *is* what it does, and
/// dropping this entry is what gives the workspace back.
struct Held<R> {
    tenant: Tenant<R>,
    /// Absent for the initial tenant, whose fence `serve` holds for the
    /// process's lifetime.
    #[cfg_attr(not(test), allow(dead_code, reason = "holding it is its only job"))]
    fence: Option<Box<dyn WorkspaceFence + Send>>,
    /// When this workspace was first observed with nothing left to do. Cleared
    /// as soon as it has work again, so the idle period a retirement waits for
    /// is continuous rather than cumulative.
    idle_since: Option<DateTime<Utc>>,
    /// Set while explicit retirement performs runtime cleanup without holding
    /// the registry lock. Resolution excludes the entry during this interval,
    /// so no new request can acquire it between the busy check and removal.
    retiring: bool,
}

#[cfg(test)]
impl<R> Held<R> {
    /// Whether this registry — rather than `serve` — holds the fence.
    fn owns_fence(&self) -> bool {
        self.fence.is_some()
    }
}

impl<F: WorkspaceFenceFactory, O: TenantRuntimeOpener> TenantRegistry<F, O>
where
    O::Runtime: Clone,
{
    /// A registry over `daemon_dir`, holding at most `limit` workspaces.
    pub fn new(daemon_dir: PathBuf, fences: F, opener: O, limit: usize) -> Self {
        Self {
            daemon_dir,
            fences,
            opener,
            limit,
            adopting: Mutex::new(BTreeSet::new()),
            adoption_changed: Condvar::new(),
            held: Mutex::new(BTreeMap::new()),
        }
    }

    /// Register the workspace this process was started in, whose fence `serve`
    /// already holds.
    ///
    /// # Errors
    ///
    /// Returns [`AdoptError::Storage`] when the state subtree or the lifecycle
    /// document cannot be opened.
    pub fn adopt_initial(&self, workspace_root: &Path) -> Result<Tenant<O::Runtime>, AdoptError> {
        self.register(workspace_root, None)
    }

    /// Take authority over `workspace_root`, or return the tenant already held
    /// for it.
    ///
    /// # Errors
    ///
    /// Returns [`AdoptError::Owned`] when another daemon fences the workspace,
    /// [`AdoptError::LimitReached`] when this daemon may hold no more, and
    /// [`AdoptError::Storage`] when the workspace's own state cannot be opened.
    pub fn adopt(&self, workspace_root: &Path) -> Result<Tenant<O::Runtime>, AdoptError> {
        let _adoption = self.adoption_permit(workspace_root);
        if let Some(tenant) = self.tenant(workspace_root) {
            return Ok(tenant);
        }
        // The fence is taken before the limit is charged and before any state is
        // written, so a refused workspace leaves this daemon exactly as it was.
        let fence = self.fences.fence_for(workspace_root);
        match fence
            .acquire()
            .map_err(|error| AdoptError::Storage(error.to_string()))?
        {
            WorkspaceFenceOutcome::Acquired => self.register(workspace_root, Some(fence)),
            WorkspaceFenceOutcome::Held { workspace, owner } => {
                Err(AdoptError::Owned { workspace, owner })
            }
        }
    }

    /// The tenant that owns `path`: the one whose root is `path` itself or its
    /// closest adopted ancestor.
    ///
    /// Comparison is by path component, so `<root>-2` is never mistaken for a
    /// child of `<root>`, and the longest match wins so a session worktree
    /// resolves to its workspace rather than to an ancestor of it.
    #[must_use]
    pub fn owner_of(&self, path: &Path) -> Option<Tenant<O::Runtime>> {
        self.with_held(|held| {
            held.values()
                .filter(|entry| !entry.retiring)
                .filter(|entry| path.starts_with(entry.tenant.root()))
                .max_by_key(|entry| entry.tenant.root().components().count())
                .map(|entry| entry.tenant.clone())
        })
    }

    /// The tenant holding `workspace_id`, which is how a fenced request names
    /// its workspace once a connection is established.
    #[must_use]
    pub fn by_workspace_id(&self, workspace_id: WorkspaceId) -> Option<Tenant<O::Runtime>> {
        self.with_held(|held| {
            held.values()
                .filter(|entry| !entry.retiring)
                .find(|entry| entry.tenant.workspace_id() == workspace_id)
                .map(|entry| entry.tenant.clone())
        })
    }

    /// The tenant adopted for exactly `workspace_root`.
    #[must_use]
    pub fn tenant(&self, workspace_root: &Path) -> Option<Tenant<O::Runtime>> {
        self.with_held(|held| {
            held.get(workspace_root)
                .filter(|entry| !entry.retiring)
                .map(|entry| entry.tenant.clone())
        })
    }

    /// Every adopted workspace, ordered by root so an inventory is stable.
    #[must_use]
    pub fn adopted(&self) -> Vec<Tenant<O::Runtime>> {
        self.with_held(|held| {
            held.values()
                .filter(|entry| !entry.retiring)
                .map(|entry| entry.tenant.clone())
                .collect()
        })
    }

    /// Atomically stop new resolutions and return the tenant to clean up.
    ///
    /// The returned handle is the caller's cleanup authority. It must finish
    /// with either [`Self::complete_retire`] or [`Self::cancel_retire`].
    ///
    /// # Errors
    ///
    /// Returns a typed refusal when the root is absent, is the process's
    /// initial tenant, or still has a connection / cleanup owner.
    pub fn begin_retire(&self, workspace_root: &Path) -> Result<Tenant<O::Runtime>, RetireError> {
        let mut held = self.lock();
        let entry = held.get_mut(workspace_root).ok_or(RetireError::NotFound)?;
        if entry.fence.is_none() {
            return Err(RetireError::Initial);
        }
        if entry.retiring || entry.tenant.referenced_elsewhere() {
            return Err(RetireError::Busy);
        }
        entry.retiring = true;
        Ok(entry.tenant.clone())
    }

    /// Commit an explicit retirement after its runtime cleanup succeeded.
    pub fn complete_retire(&self, workspace_root: &Path) -> bool {
        let mut held = self.lock();
        if held.get(workspace_root).is_some_and(|entry| entry.retiring) {
            held.remove(workspace_root);
            true
        } else {
            false
        }
    }

    /// Re-open resolution when explicit runtime cleanup failed.
    pub fn cancel_retire(&self, workspace_root: &Path) {
        if let Some(entry) = self.lock().get_mut(workspace_root) {
            entry.retiring = false;
        }
    }

    /// Give `workspace_root` back, releasing its fence.
    ///
    /// Returns whether a tenant was held. The initial tenant is released too,
    /// but its fence belongs to `serve`, so the workspace stays owned by this
    /// process until it exits.
    pub fn retire(&self, workspace_root: &Path) -> bool {
        self.lock().remove(workspace_root).is_some()
    }

    /// Give back every workspace that has had nothing to do for `idle_for`.
    ///
    /// Returns the roots that were released, for the caller to log. A workspace
    /// is released only when all four of these hold, and the last two are
    /// re-checked on every sweep rather than trusted from an earlier one:
    ///
    /// * it is not the workspace `serve` fenced for this process — that fence
    ///   belongs to the process, so retiring the tenant would drop the runtime
    ///   without giving the workspace back;
    /// * nothing outside the registry still holds it;
    /// * [`WorkspaceActivity`] reports no work of its own;
    /// * it has been in that state continuously for `idle_for`.
    pub fn retire_idle(
        &self,
        activity: &dyn WorkspaceActivity<O::Runtime>,
        now: DateTime<Utc>,
        idle_for: chrono::Duration,
    ) -> Vec<PathBuf> {
        // The candidates, chosen under the lock. The tenant handle is never
        // cloned here: a clone would itself be an outside reference and hide the
        // very condition this reads. The initial tenant is excluded because its
        // fence is the process's — releasing the entry would drop the runtime
        // without giving the workspace back.
        let candidates = {
            let held = self.lock();
            held.iter()
                .filter(|(_, entry)| {
                    entry.fence.is_some() && !entry.retiring && !entry.tenant.referenced_elsewhere()
                })
                .map(|(root, entry)| {
                    (
                        root.clone(),
                        entry.tenant.workspace_id(),
                        entry.tenant.runtime().clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        // Observed with the lock released. `has_work` reaches the daemon-wide
        // runtimes, and a request already inside one of those takes *its* lock
        // before resolving a workspace through this registry. Holding the
        // registry lock across the observation would invert that order, and the
        // two threads would wait on each other for the life of the process.
        let idle = candidates
            .into_iter()
            .filter(|(_, workspace, runtime)| !activity.has_work(*workspace, runtime))
            .map(|(root, _, _)| root)
            .collect::<std::collections::BTreeSet<_>>();
        // Committed under the lock, re-reading everything that could have
        // changed while it was released: a workspace adopted, referenced, or
        // given work again in that window must not be retired on a stale
        // observation.
        let mut held = self.lock();
        let mut retired = Vec::new();
        for (root, entry) in held.iter_mut() {
            let still_idle = idle.contains(root)
                && entry.fence.is_some()
                && !entry.retiring
                && !entry.tenant.referenced_elsewhere();
            if still_idle {
                let since = *entry.idle_since.get_or_insert(now);
                if now.signed_duration_since(since) >= idle_for {
                    retired.push(root.clone());
                }
            } else {
                entry.idle_since = None;
            }
        }
        for root in &retired {
            held.remove(root);
        }
        retired
    }

    /// Open the workspace's state and record it as held.
    fn register(
        &self,
        workspace_root: &Path,
        fence: Option<Box<dyn WorkspaceFence + Send>>,
    ) -> Result<Tenant<O::Runtime>, AdoptError> {
        let state = workspace_state::resolve(&self.daemon_dir, workspace_root)
            .map_err(|error| AdoptError::Storage(format!("{error:#}")))?;
        let opened = self
            .opener
            .open(workspace_root, state.dir())
            .map_err(|error| AdoptError::Storage(error.to_string()))?;
        let tenant = Tenant {
            shared: std::sync::Arc::new(TenantIdentity {
                root: workspace_root.to_path_buf(),
                state_dir: state.dir().to_path_buf(),
                workspace_id: opened.workspace_id,
                runtime: opened.runtime,
            }),
        };
        let mut held = self.lock();
        // The limit is charged here, under the same lock that inserts, so two
        // concurrent adoptions cannot both see room for the last slot.
        if !held.contains_key(workspace_root) && held.len() >= self.limit {
            return Err(AdoptError::LimitReached { limit: self.limit });
        }
        held.insert(
            workspace_root.to_path_buf(),
            Held {
                tenant: tenant.clone(),
                fence,
                idle_since: None,
                retiring: false,
            },
        );
        Ok(tenant)
    }

    /// Enter the one adoption lane for `workspace_root` while allowing every
    /// other root to proceed. The waiter rechecks the tenant after entering, so
    /// the first adopter's result is reused without opening another fence.
    fn adoption_permit(&self, workspace_root: &Path) -> AdoptionPermit<'_> {
        let mut adopting = self
            .adopting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while adopting.contains(workspace_root) {
            adopting = self
                .adoption_changed
                .wait(adopting)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        adopting.insert(workspace_root.to_path_buf());
        AdoptionPermit {
            root: workspace_root.to_path_buf(),
            adopting: &self.adopting,
            changed: &self.adoption_changed,
        }
    }

    /// Whether the registry lock is free right now.
    ///
    /// Used only by the regression test that pins the observation in
    /// [`Self::retire_idle`] outside the lock.
    #[cfg(test)]
    fn is_unlocked(&self) -> bool {
        self.held.try_lock().is_ok()
    }

    fn with_held<T>(&self, read: impl FnOnce(&BTreeMap<PathBuf, Held<O::Runtime>>) -> T) -> T {
        read(&self.lock())
    }

    /// A poisoned registry is still the authority on what this process holds:
    /// the fences are held by live objects either way, so recovering the map is
    /// strictly better than refusing every workspace for the rest of the
    /// process's life.
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<PathBuf, Held<O::Runtime>>> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl<F, O> WorkspaceRuntimes for TenantRegistry<F, O>
where
    F: WorkspaceFenceFactory + Send + Sync,
    O: TenantRuntimeOpener<Runtime = SharedSessionRuntime> + Send + Sync,
{
    fn workspace(&self, workspace: WorkspaceId) -> Option<Tenant<SharedSessionRuntime>> {
        self.by_workspace_id(workspace)
    }

    fn workspace_at(&self, root: &Path) -> Option<Tenant<SharedSessionRuntime>> {
        self.tenant(root)
    }

    fn owner_of_path(&self, path: &Path) -> Option<Tenant<SharedSessionRuntime>> {
        self.owner_of(path)
    }

    fn all(&self) -> Vec<Tenant<SharedSessionRuntime>> {
        self.adopted()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    /// A fence that answers what the test asked for and remembers whether it is
    /// still alive, so a retirement can be observed as a released workspace.
    struct FakeFence {
        outcome: WorkspaceFenceOutcome,
        live: std::sync::Arc<AtomicUsize>,
    }

    impl WorkspaceFence for FakeFence {
        fn acquire(&self) -> io::Result<WorkspaceFenceOutcome> {
            Ok(self.outcome.clone())
        }
    }

    impl Drop for FakeFence {
        fn drop(&mut self) {
            self.live.fetch_sub(1, Ordering::SeqCst);
        }
    }

    struct FakeFences {
        outcome: WorkspaceFenceOutcome,
        live: std::sync::Arc<AtomicUsize>,
        failure: Option<std::io::ErrorKind>,
    }

    struct FailingFence(std::io::ErrorKind);

    impl WorkspaceFence for FailingFence {
        fn acquire(&self) -> io::Result<WorkspaceFenceOutcome> {
            Err(io::Error::new(self.0, "fence node is unusable"))
        }
    }

    impl WorkspaceFenceFactory for FakeFences {
        fn fence_for(&self, _: &Path) -> Box<dyn WorkspaceFence + Send> {
            if let Some(kind) = self.failure {
                return Box::new(FailingFence(kind));
            }
            self.live.fetch_add(1, Ordering::SeqCst);
            Box::new(FakeFence {
                outcome: self.outcome.clone(),
                live: std::sync::Arc::clone(&self.live),
            })
        }
    }

    /// A runtime that is just the workspace it was opened for, which is all the
    /// registry may assume about it.
    struct FakeOpener {
        fail: AtomicBool,
    }

    impl TenantRuntimeOpener for FakeOpener {
        type Runtime = String;

        fn open(
            &self,
            workspace_root: &Path,
            state_dir: &Path,
        ) -> io::Result<OpenedTenant<String>> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(io::Error::other("lifecycle document is unreadable"));
            }
            assert!(state_dir.is_dir(), "the subtree is created before opening");
            Ok(OpenedTenant {
                runtime: workspace_root.display().to_string(),
                workspace_id: WorkspaceId::new(),
            })
        }
    }

    fn fixture(
        daemon_dir: &Path,
        outcome: WorkspaceFenceOutcome,
        limit: usize,
    ) -> (
        TenantRegistry<FakeFences, FakeOpener>,
        std::sync::Arc<AtomicUsize>,
    ) {
        let live = std::sync::Arc::new(AtomicUsize::new(0));
        let registry = TenantRegistry::new(
            daemon_dir.to_path_buf(),
            FakeFences {
                outcome,
                live: std::sync::Arc::clone(&live),
                failure: None,
            },
            FakeOpener {
                fail: AtomicBool::new(false),
            },
            limit,
        );
        (registry, live)
    }

    #[test]
    fn adoption_fences_a_workspace_opens_its_state_and_is_idempotent() {
        let daemon = tempfile::tempdir_in("/tmp").unwrap();
        let (registry, live) = fixture(daemon.path(), WorkspaceFenceOutcome::Acquired, 8);
        let root = Path::new("/workspace/one");

        let tenant = registry.adopt(root).unwrap();
        assert_eq!(tenant.root(), root);
        assert_eq!(tenant.runtime(), "/workspace/one");
        assert_eq!(
            tenant.state_dir(),
            workspace_state::resolve(daemon.path(), root).unwrap().dir()
        );
        assert_eq!(live.load(Ordering::SeqCst), 1);

        // Adopting again is the same tenant, and does not take a second fence
        // over a workspace this daemon already owns.
        assert_eq!(registry.adopt(root).unwrap(), tenant);
        assert_eq!(live.load(Ordering::SeqCst), 1);
        assert!(registry.lock()[root].owns_fence());
        assert_eq!(registry.adopted(), vec![tenant.clone()]);
        assert_eq!(registry.tenant(root), Some(tenant.clone()));
        assert_eq!(
            registry.by_workspace_id(tenant.workspace_id()),
            Some(tenant)
        );
        assert_eq!(registry.by_workspace_id(WorkspaceId::new()), None);
    }

    #[test]
    fn concurrent_first_adoptions_converge_on_one_tenant_and_fence() {
        let daemon = tempfile::tempdir_in("/tmp").unwrap();
        let (registry, live) = fixture(daemon.path(), WorkspaceFenceOutcome::Acquired, 8);
        let registry = std::sync::Arc::new(registry);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(17));
        let mut workers = Vec::new();
        for _ in 0..16 {
            let registry = std::sync::Arc::clone(&registry);
            let barrier = std::sync::Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                registry.adopt(Path::new("/workspace/concurrent")).unwrap()
            }));
        }
        barrier.wait();
        let tenants = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();

        assert!(tenants.iter().all(|tenant| tenant == &tenants[0]));
        assert_eq!(live.load(Ordering::SeqCst), 1);
        assert_eq!(registry.adopted(), vec![tenants[0].clone()]);
    }

    #[test]
    fn the_initial_tenant_is_registered_without_taking_a_second_fence() {
        let daemon = tempfile::tempdir_in("/tmp").unwrap();
        // The outcome would refuse, which proves the fence is never consulted:
        // `serve` already holds this workspace for the process's lifetime.
        let (registry, live) = fixture(
            daemon.path(),
            WorkspaceFenceOutcome::Held {
                workspace: "/workspace/one".into(),
                owner: Some(7),
            },
            8,
        );
        let root = Path::new("/workspace/one");

        let tenant = registry.adopt_initial(root).unwrap();
        assert_eq!(tenant.root(), root);
        assert_eq!(live.load(Ordering::SeqCst), 0);
        assert!(!registry.lock()[root].owns_fence());
        // And it is a tenant like any other from then on.
        assert_eq!(registry.adopt(root).unwrap(), tenant);
    }

    #[test]
    fn a_workspace_another_daemon_owns_is_refused_alone() {
        let daemon = tempfile::tempdir_in("/tmp").unwrap();
        let (registry, _live) = fixture(daemon.path(), WorkspaceFenceOutcome::Acquired, 8);
        let held = registry.adopt(Path::new("/workspace/one")).unwrap();

        let refusing = TenantRegistry::new(
            daemon.path().to_path_buf(),
            FakeFences {
                outcome: WorkspaceFenceOutcome::Held {
                    workspace: "/workspace/two".into(),
                    owner: Some(4242),
                },
                live: std::sync::Arc::new(AtomicUsize::new(0)),
                failure: None,
            },
            FakeOpener {
                fail: AtomicBool::new(false),
            },
            8,
        );
        let error = refusing.adopt(Path::new("/workspace/two")).unwrap_err();
        assert_eq!(
            error,
            AdoptError::Owned {
                workspace: "/workspace/two".into(),
                owner: Some(4242)
            }
        );
        assert!(error.to_string().contains("pid 4242"), "{error}, {error:?}");
        // The composition root speaks `io::Error`; the message survives.
        assert_eq!(
            io::Error::from(error.clone()).to_string(),
            error.to_string()
        );
        assert!(
            AdoptError::Owned {
                workspace: "/workspace/two".into(),
                owner: None
            }
            .to_string()
            .contains("/workspace/two")
        );

        // The refusal is about that workspace only: what this daemon already
        // holds is untouched, and nothing was written for the refused one.
        assert_eq!(registry.adopted(), vec![held]);
        assert!(refusing.adopted().is_empty());
    }

    #[test]
    fn an_unusable_fence_and_an_unreadable_document_are_reported_as_storage() {
        let daemon = tempfile::tempdir_in("/tmp").unwrap();
        let unusable = TenantRegistry::new(
            daemon.path().to_path_buf(),
            FakeFences {
                outcome: WorkspaceFenceOutcome::Acquired,
                live: std::sync::Arc::new(AtomicUsize::new(0)),
                failure: Some(io::ErrorKind::PermissionDenied),
            },
            FakeOpener {
                fail: AtomicBool::new(false),
            },
            8,
        );
        let error = unusable.adopt(Path::new("/workspace/one")).unwrap_err();
        assert_eq!(error, AdoptError::Storage("fence node is unusable".into()));
        assert_eq!(error.to_string(), "fence node is unusable");

        let (registry, _live) = fixture(daemon.path(), WorkspaceFenceOutcome::Acquired, 8);
        registry.opener.fail.store(true, Ordering::SeqCst);
        assert_eq!(
            registry.adopt(Path::new("/workspace/one")).unwrap_err(),
            AdoptError::Storage("lifecycle document is unreadable".into())
        );

        // A state subtree that cannot be resolved is the same kind of refusal.
        let broken = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::write(
            broken
                .path()
                .join(usagi_core::infrastructure::paths::WORKSPACE_STATE_DIR),
            "",
        )
        .unwrap();
        let (blocked, _live) = fixture(broken.path(), WorkspaceFenceOutcome::Acquired, 8);
        let error = blocked.adopt(Path::new("/workspace/one")).unwrap_err();
        assert!(
            matches!(&error, AdoptError::Storage(reason) if reason.contains("root.json")),
            "{error:?}"
        );
    }

    #[test]
    fn the_limit_refuses_a_new_workspace_and_still_admits_a_held_one() {
        let daemon = tempfile::tempdir_in("/tmp").unwrap();
        let (registry, _live) = fixture(daemon.path(), WorkspaceFenceOutcome::Acquired, 1);
        let first = registry.adopt(Path::new("/workspace/one")).unwrap();

        assert_eq!(
            registry.adopt(Path::new("/workspace/two")).unwrap_err(),
            AdoptError::LimitReached { limit: 1 }
        );
        assert!(
            AdoptError::LimitReached { limit: 1 }
                .to_string()
                .contains("retire one")
        );
        assert_eq!(registry.adopted(), vec![first]);

        // Giving one back makes room again.
        assert!(registry.retire(Path::new("/workspace/one")));
        assert!(!registry.retire(Path::new("/workspace/one")));
        registry.adopt(Path::new("/workspace/two")).unwrap();
        assert_eq!(registry.adopted().len(), 1);
    }

    #[test]
    fn retiring_a_tenant_releases_its_fence() {
        let daemon = tempfile::tempdir_in("/tmp").unwrap();
        let (registry, live) = fixture(daemon.path(), WorkspaceFenceOutcome::Acquired, 8);
        registry.adopt(Path::new("/workspace/one")).unwrap();
        assert_eq!(live.load(Ordering::SeqCst), 1);

        assert!(registry.retire(Path::new("/workspace/one")));
        assert_eq!(live.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn explicit_retirement_is_typed_and_excludes_the_root_during_cleanup() {
        let daemon = tempfile::tempdir_in("/tmp").unwrap();
        let (registry, live) = fixture(daemon.path(), WorkspaceFenceOutcome::Acquired, 8);
        let initial = registry
            .adopt_initial(Path::new("/workspace/initial"))
            .unwrap();
        let held = registry.adopt(Path::new("/workspace/one")).unwrap();

        assert_eq!(
            registry.begin_retire(Path::new("/workspace/missing")),
            Err(RetireError::NotFound)
        );
        assert_eq!(
            registry.begin_retire(Path::new("/workspace/initial")),
            Err(RetireError::Initial)
        );
        assert_eq!(
            registry.begin_retire(Path::new("/workspace/one")),
            Err(RetireError::Busy),
            "an outstanding connection handle must fence retirement"
        );
        drop((initial, held));

        let cleanup = registry.begin_retire(Path::new("/workspace/one")).unwrap();
        assert!(registry.tenant(Path::new("/workspace/one")).is_none());
        assert_eq!(registry.adopted().len(), 1);
        assert_eq!(
            registry.adopted()[0].root(),
            Path::new("/workspace/initial")
        );
        assert_eq!(
            registry.begin_retire(Path::new("/workspace/one")),
            Err(RetireError::Busy)
        );
        registry.cancel_retire(Path::new("/workspace/one"));
        drop(cleanup);
        assert!(registry.tenant(Path::new("/workspace/one")).is_some());

        let cleanup = registry.begin_retire(Path::new("/workspace/one")).unwrap();
        assert!(registry.complete_retire(Path::new("/workspace/one")));
        assert!(!registry.complete_retire(Path::new("/workspace/one")));
        drop(cleanup);
        assert_eq!(live.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_owner_of_a_path_is_its_closest_adopted_ancestor() {
        let daemon = tempfile::tempdir_in("/tmp").unwrap();
        let (registry, _live) = fixture(daemon.path(), WorkspaceFenceOutcome::Acquired, 8);
        registry.adopt(Path::new("/workspace/one")).unwrap();
        registry.adopt(Path::new("/workspace/one/nested")).unwrap();

        for (candidate, expected) in [
            ("/workspace/one", "/workspace/one"),
            ("/workspace/one/.usagi/sessions/work", "/workspace/one"),
            ("/workspace/one/nested/deep", "/workspace/one/nested"),
        ] {
            let owner = registry.owner_of(Path::new(candidate)).unwrap();
            assert_eq!(owner.root(), Path::new(expected), "{candidate}");
        }
        // A sibling that only shares a spelling prefix is not a child, and an
        // unadopted workspace has no owner.
        assert_eq!(registry.owner_of(Path::new("/workspace/one-2")), None);
        assert_eq!(registry.owner_of(Path::new("/elsewhere")), None);
    }

    /// A retirement gives a workspace back only when nothing is left that could
    /// need it: no live work, no holder, and a continuous idle period. Each of
    /// those is checked on every sweep, so a workspace that becomes busy again
    /// restarts the clock instead of being released on an old observation.
    #[test]
    fn retirement_waits_for_a_continuous_idle_period_with_no_work_and_no_holder() {
        struct Busy(std::cell::Cell<bool>);
        impl WorkspaceActivity<String> for Busy {
            fn has_work(&self, _: WorkspaceId, _: &String) -> bool {
                self.0.get()
            }
        }

        let daemon = tempfile::tempdir_in("/tmp").unwrap();
        let live = std::sync::Arc::new(AtomicUsize::new(0));
        let registry = TenantRegistry::new(
            daemon.path().to_path_buf(),
            FakeFences {
                outcome: WorkspaceFenceOutcome::Acquired,
                live: std::sync::Arc::clone(&live),
                failure: None,
            },
            FakeOpener {
                fail: AtomicBool::new(false),
            },
            8,
        );
        let start = DateTime::parse_from_rfc3339("2026-08-24T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let idle_for = chrono::Duration::minutes(10);
        let activity = Busy(std::cell::Cell::new(false));

        // The initial tenant is never retired: its fence belongs to `serve`, so
        // releasing the entry would drop the runtime without giving the
        // workspace back.
        let initial = registry
            .adopt_initial(Path::new("/workspace/initial"))
            .unwrap();
        let adopted = registry.adopt(Path::new("/workspace/one")).unwrap();
        assert_eq!(live.load(Ordering::SeqCst), 1);

        // A held handle keeps the workspace, however long it has been quiet.
        assert!(
            registry
                .retire_idle(&activity, start + chrono::Duration::hours(1), idle_for)
                .is_empty()
        );
        drop(adopted);

        // Work of its own keeps it too, and restarts the idle clock.
        activity.0.set(true);
        assert!(registry.retire_idle(&activity, start, idle_for).is_empty());
        activity.0.set(false);
        assert!(registry.retire_idle(&activity, start, idle_for).is_empty());
        activity.0.set(true);
        assert!(
            registry
                .retire_idle(&activity, start + idle_for, idle_for)
                .is_empty(),
            "work must restart the idle period"
        );
        activity.0.set(false);
        assert!(
            registry
                .retire_idle(&activity, start + idle_for, idle_for)
                .is_empty()
        );

        // Only a continuous idle period of the full length releases it, and the
        // fence goes back with it.
        let retired = registry.retire_idle(&activity, start + idle_for * 2, idle_for);
        assert_eq!(retired, vec![PathBuf::from("/workspace/one")]);
        assert_eq!(live.load(Ordering::SeqCst), 0);
        assert_eq!(registry.adopted(), vec![initial]);
    }

    /// The observation must not run under the registry lock.
    ///
    /// `has_work` reaches the daemon-wide PTY and Agent runtimes, and a request
    /// already inside one of those takes *its* lock and then resolves a
    /// workspace through this registry. If a retirement held the registry lock
    /// while observing, the two orders would invert and both threads would wait
    /// on each other for the life of the process — every workspace's client
    /// frozen, with no recovery but a restart.
    #[test]
    fn the_activity_observation_does_not_hold_the_registry_lock() {
        struct Observer {
            registry:
                std::cell::RefCell<Option<std::rc::Weak<TenantRegistry<FakeFences, FakeOpener>>>>,
            observed: Cell<usize>,
            unlocked: Cell<bool>,
        }
        impl WorkspaceActivity<String> for Observer {
            fn has_work(&self, _: WorkspaceId, _: &String) -> bool {
                let registry = self
                    .registry
                    .borrow()
                    .as_ref()
                    .and_then(std::rc::Weak::upgrade)
                    .expect("the registry outlives the observation");
                self.observed.set(self.observed.get() + 1);
                // Reading the registry is exactly what the real observation does
                // transitively, through a runtime that resolves workspaces.
                self.unlocked
                    .set(self.unlocked.get() && registry.is_unlocked());
                false
            }
        }

        let daemon = tempfile::tempdir_in("/tmp").unwrap();
        // `Rc` rather than `Arc`: the fake opener is single-threaded, and the
        // observation this pins happens on the caller's thread.
        let registry = std::rc::Rc::new(TenantRegistry::new(
            daemon.path().to_path_buf(),
            FakeFences {
                outcome: WorkspaceFenceOutcome::Acquired,
                live: std::sync::Arc::new(AtomicUsize::new(0)),
                failure: None,
            },
            FakeOpener {
                fail: AtomicBool::new(false),
            },
            8,
        ));
        let observer = Observer {
            registry: std::cell::RefCell::new(Some(std::rc::Rc::downgrade(&registry))),
            observed: Cell::new(0),
            unlocked: Cell::new(true),
        };
        registry.adopt(Path::new("/workspace/one")).unwrap();
        registry.adopt(Path::new("/workspace/two")).unwrap();

        let retired = registry.retire_idle(&observer, Utc::now(), chrono::Duration::zero());

        assert_eq!(observer.observed.get(), 2, "every candidate is observed");
        assert!(
            observer.unlocked.get(),
            "the registry lock must be free while a workspace is observed"
        );
        assert_eq!(retired.len(), 2);
    }

    #[test]
    fn a_poisoned_registry_still_answers_for_what_it_holds() {
        let daemon = tempfile::tempdir_in("/tmp").unwrap();
        let (registry, _live) = fixture(daemon.path(), WorkspaceFenceOutcome::Acquired, 8);
        let tenant = registry.adopt(Path::new("/workspace/one")).unwrap();

        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = registry.held.lock().unwrap();
            panic!("a reader panicked while holding the registry");
        }));
        assert!(poisoned.is_err());

        // The fences are held by live objects either way, so refusing every
        // workspace for the rest of the process would lose more than it saves.
        assert_eq!(registry.adopted(), vec![tenant]);
    }
}
