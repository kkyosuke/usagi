//! Create and remove sessions: parallel working trees under
//! `.usagi/sessions/<name>/`.
//!
//! The workspace root need not itself be a git repository. The root is walked
//! recursively: every git repository found gets a fresh `git worktree` (on a new
//! branch `usagi/<name>`, the session name under the [`BRANCH_PREFIX`] namespace)
//! at its mirrored location under
//! `.usagi/sessions/<name>/`, while non-git files and directories are copied
//! there. This supports a single repository, or a tree containing several — e.g.
//!
//! ```text
//! /root            (not a repo)
//! ├── app-a/  =git → worktree
//! ├── be/          (plain dir → recurse)
//! │   └── be1/=git → worktree
//! └── README.md   → copied
//! ```
//!
//! This module owns the session lifecycle and state recording. The recursive
//! mirroring and repository discovery live in [`tree`]; reconciling the on-disk
//! tree with `state.json` lives in [`reconcile`].

mod reconcile;
mod tree;

pub use reconcile::reconcile;

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};

use crate::domain::agent::Agent;
use crate::domain::settings::LocalSettings;
use crate::domain::workspace_state::{PrLink, SessionRecord};
use crate::infrastructure::repo_paths::{SESSIONS_DIR, STATE_DIR};
use crate::infrastructure::setup_runner::SystemSetupCommandRunner;
use crate::infrastructure::workspace_store::WorkspaceStore;
use crate::infrastructure::{
    agent_prompt_store, agent_state_store, git, open_panes_store, pr_link_store,
};
use crate::usecase::workspace_state;

/// The namespace every session's git branch lives under: a session named
/// `<name>` checks out the branch `usagi/<name>` in each repository.
///
/// Prefixing keeps usagi-managed branches from colliding with the branches a
/// developer cuts by hand (a bare `<name>`, a `feat/…`, …): everything usagi
/// creates is corralled under `usagi/`. Only the *branch* is namespaced — the
/// session name itself (the directory under `.usagi/sessions/`, the `state.json`
/// record, the sidebar label) stays unprefixed.
pub const BRANCH_PREFIX: &str = "usagi/";

/// The git branch a session named `name` checks out: `name` under the
/// [`BRANCH_PREFIX`] namespace. This is the single source of truth mapping a
/// session name to its branch, shared by [`create`] (cutting the branch),
/// [`remove`]/[`reconcile`] (dropping it), and the TUI's live-create validation.
pub fn branch_name(name: &str) -> String {
    format!("{BRANCH_PREFIX}{name}")
}

/// The outcome of creating a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedSession {
    /// The session name (the branch it cuts in every repository is
    /// [`branch_name`]`(name)`, i.e. `usagi/<name>`).
    pub name: String,
    /// Root of the session tree: `<workspace>/.usagi/sessions/<name>`.
    pub root: PathBuf,
    /// The mirrored path of every repository that received a new worktree.
    pub worktrees: Vec<PathBuf>,
}

/// Runs one configured session setup command.
///
/// Abstracted so session creation can be tested without launching arbitrary
/// shell commands. The production implementation executes the command line via
/// the platform shell with the new session root as its current directory.
pub trait SetupCommandRunner {
    fn run(&self, cwd: &Path, command: &str) -> Result<()>;
}

/// Create session `name` under `workspace_root`.
///
/// Fails if the name is empty or contains path separators, or if the session
/// already exists. Any git error (e.g. the branch already exists in a repo) is
/// surfaced.
pub fn create(workspace_root: &Path, name: &str) -> Result<CreatedSession> {
    create_with_setup_runner(workspace_root, name, &SystemSetupCommandRunner)
}

fn create_with_setup_runner(
    workspace_root: &Path,
    name: &str,
    setup_runner: &dyn SetupCommandRunner,
) -> Result<CreatedSession> {
    let name = name.trim();
    if name.is_empty() {
        bail!("session name must not be empty");
    }
    if let Some(error) = name_format_error(name) {
        bail!("{error}");
    }

    let store = WorkspaceStore::new(workspace_root);
    // Hold the store lock across the entire create — reconcile → build the
    // worktree → record — so a concurrent `create`/`remove` (which reconciles)
    // cannot observe this half-built, not-yet-recorded worktree as a stray and
    // force-remove it (destroying freshly built work and leaving a ghost
    // record). The lock is released when `create` returns. The trade-off is
    // that a long worktree build holds the lock for its duration; correctness
    // wins over the rare lock-wait timeout.
    let _lock = store.lock()?;

    // Sync the on-disk tree with the recorded sessions first: a leftover
    // directory `state.json` does not know about is force-removed, so a stale
    // directory of the same name never blocks a fresh session.
    reconcile::reconcile_locked(workspace_root)?;

    let dest_root = workspace_root.join(STATE_DIR).join(SESSIONS_DIR).join(name);
    if dest_root.exists() {
        bail!("session \"{name}\" already exists");
    }

    // A session creates the branch `usagi/<name>` (see [`branch_name`]) in every
    // source repository. If a repo already has branches nested under that branch
    // (e.g. a hand-made `usagi/<name>/foo`), git cannot create `usagi/<name>` and
    // fails partway with a cryptic `cannot lock ref` error. Refuse up front with a
    // clear, actionable message before touching any repository.
    let branch = branch_name(name);
    for repo in tree::source_repos(workspace_root) {
        // Clear any dangling worktree registration whose directory was deleted
        // out-of-band (a crash, a manual `rm`, or a teardown that left a worktree
        // on an unexpected branch registered). Without this, `git worktree add`
        // at the reused session path fails with "missing but already registered
        // worktree" and the session can never be recreated. Best-effort: a prune
        // failure must not block creation, so it is logged-and-ignored.
        let _ = git::prune_worktrees(&repo);
        if let Some(conflict) = git::branch_namespace_conflict(&repo, &branch) {
            bail!(
                "session \"{name}\" conflicts with the existing branch \"{conflict}\": \
                 the branch \"{branch}\" cannot be created alongside branches under \
                 \"{branch}/\". Choose a different session name."
            );
        }
    }

    let mut worktrees = Vec::new();
    if tree::is_repo_root(workspace_root) {
        // The whole workspace is one repository: a single worktree at the root.
        let parent = dest_root
            .parent()
            .expect("dest_root always has a .usagi/sessions parent");
        fs::create_dir_all(parent).context(format!("failed to create {}", parent.display()))?;
        let base = tree::base_ref(workspace_root);
        git::add_worktree(workspace_root, &dest_root, &branch, base.as_deref())?;
        git::init_submodules(&dest_root)?;
        worktrees.push(dest_root.clone());
    } else {
        fs::create_dir_all(&dest_root)
            .context(format!("failed to create {}", dest_root.display()))?;
        tree::build_dir(workspace_root, &dest_root, &branch, &mut worktrees)?;
    }

    // Symlink usagi's shipped skills into each worktree so the agent launched
    // there discovers them. The skills themselves are materialised once under the
    // global data dir at startup (see
    // [`skills::materialize`](crate::infrastructure::skills::materialize)); this
    // points each worktree's `.claude/skills/<name>` at that directory and
    // excludes those symlinks from git so they never mark the session dirty.
    // Only the skills whose feature is enabled in the workspace's effective
    // settings are linked; a settings read failure falls back to the defaults
    // (every feature on). Best-effort: a skills hiccup must not fail an
    // otherwise-built session.
    let skill_settings =
        crate::usecase::settings::effective_for(workspace_root).unwrap_or_default();
    let skill_excludes = crate::infrastructure::skills::git_exclude_patterns();
    for wt in &worktrees {
        for pattern in &skill_excludes {
            let _ = git::ensure_excluded(wt, pattern);
        }
        let _ = crate::infrastructure::skills::link(wt, &skill_settings);
    }

    let local_settings = crate::usecase::settings::load_local(workspace_root).unwrap_or_default();

    // Record the session *before* running setup, then release the store lock so
    // the (arbitrary, possibly minutes-long) user setup commands do not hold it.
    // Holding the lock across e.g. `npm ci` would make every concurrent
    // create/remove and background `workspace_state::sync` fail on the
    // lock-acquire timeout. Recording first keeps reconcile from mistaking this
    // now-registered worktree for a stray while setup runs; a setup failure is
    // logged, never rolled back (the worktree already exists for the user to fix).
    record(&store, name, &dest_root, &worktrees)?;
    drop(_lock);

    run_setup_commands(&dest_root, name, &local_settings, setup_runner);

    crate::infrastructure::trace_log::TraceLog::record(
        crate::domain::trace::TraceEvent::now(
            crate::domain::trace::TraceCategory::Session,
            "create",
        )
        .with_detail(name),
    );

    Ok(CreatedSession {
        name: name.to_string(),
        root: dest_root,
        worktrees,
    })
}

/// Run the workspace's configured setup commands in the newly-created session
/// root. Failures are logged and traced, but they do not roll back the session:
/// at this point the worktree exists and the user can inspect/fix the setup
/// command from inside it.
fn run_setup_commands(
    session_root: &Path,
    session_name: &str,
    settings: &LocalSettings,
    runner: &dyn SetupCommandRunner,
) {
    for command in settings.setup_commands() {
        crate::infrastructure::trace_log::TraceLog::record(
            crate::domain::trace::TraceEvent::now(
                crate::domain::trace::TraceCategory::Session,
                "setup_command",
            )
            .with_detail(format!("{session_name}: {command}")),
        );
        if let Err(error) = runner.run(session_root, command) {
            crate::infrastructure::error_log::ErrorLog::record(&format!(
                "session setup command failed for {session_name} in {}: {error:#}",
                session_root.display()
            ));
            crate::infrastructure::trace_log::TraceLog::record(
                crate::domain::trace::TraceEvent::now(
                    crate::domain::trace::TraceCategory::Session,
                    "setup_command_failed",
                )
                .with_detail(format!("{session_name}: {command}")),
            );
        }
    }
}

/// The reason a session name breaks a structural rule, or `None` when its format
/// is acceptable. This is the single source of truth for what makes a name
/// legal, shared by [`create`] (which also rejects an empty name and checks for
/// existing sessions / branch clashes that need disk and git access) and the
/// TUI's live inline-create validation, so the two never drift.
///
/// A session name becomes a git branch name and a directory under
/// `.usagi/sessions/`, so it must not contain a path separator (`/`, `\`, `.`,
/// `..`) and must not start with `-` — a leading `-` would be parsed by git as an
/// option (e.g. `-D`) where the name is interpolated into `git branch -D <name>`
/// / `git worktree add -b <name>`.
///
/// An empty name has no bad characters and so passes here; callers decide whether
/// emptiness itself is an error ([`create`] rejects it; the TUI stays quiet while
/// nothing is typed).
pub fn name_format_error(name: &str) -> Option<String> {
    let name = name.trim();
    if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return Some("session name must not contain path separators".to_string());
    }
    if name.starts_with('-') {
        return Some("session name must not start with \"-\"".to_string());
    }
    None
}

/// The local branch names that already exist across every source repository a
/// session under `workspace_root` would span, de-duplicated and sorted.
///
/// A new session cuts a `<name>` branch in each of these repos, so this is the
/// set its name must avoid — both as an exact duplicate and as a namespace
/// clash (a branch under `<name>/`). The TUI reads it once when the inline
/// create input opens to validate the typed name live (see
/// [`git::branch_namespace_conflict`]). Best-effort: a non-git or unreadable
/// repo simply contributes no names.
pub fn existing_branch_names(workspace_root: &Path) -> Vec<String> {
    use std::collections::BTreeSet;
    tree::source_repos(workspace_root)
        .iter()
        .flat_map(|repo| git::local_branches(repo))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Append the session to `<workspace>/.usagi/state.json`, creating the state
/// when none exists yet. This is what lets a multi-repo, non-git root still
/// track its sessions. Each worktree's git status is captured at record time.
fn record(store: &WorkspaceStore, name: &str, root: &Path, worktrees: &[PathBuf]) -> Result<()> {
    // The caller ([`create`]) holds the store lock across the whole operation,
    // so the load → append → save here is already serialised against any other
    // process mutating this workspace's `state.json`.
    let mut state = store.load()?.unwrap_or_default();

    // A session's worktrees may live in different source repositories (a
    // multi-repo workspace); the shared helper classifies each against its own
    // repository's default branch, resolved once per repository.
    let worktree_states = workspace_state::inspect_worktrees(worktrees);

    let now = Utc::now();
    state.sessions.push(SessionRecord {
        name: name.to_string(),
        display_name: None,
        note: None,
        root: root.to_path_buf(),
        worktrees: worktree_states,
        created_at: now,
        last_active: None,
    });
    state.updated_at = now;
    store.save(&state)
}

/// Run `edit` against the session named `name`, then persist the change,
/// holding the store lock across the whole load → edit → save.
///
/// This is the single home of the locking discipline shared by
/// [`set_display_name`] and [`set_note`]: holding the lock across the
/// read-modify-write keeps a concurrent writer from clobbering the edit (or
/// having it clobber theirs), and `updated_at` is bumped and the state saved in
/// one place so the two callers cannot drift. `edit` mutates the matched session
/// and returns the value to hand back. Fails when no state is recorded or no
/// session named `name` exists.
///
/// [`reorder`] does *not* use this: its no-op (a move past either end) must
/// leave `state.json` untouched, whereas this always saves.
fn edit_session<T>(
    store: &WorkspaceStore,
    name: &str,
    edit: impl FnOnce(&mut SessionRecord) -> T,
) -> Result<T> {
    let _lock = store.lock()?;
    let mut state = store
        .load()?
        .ok_or_else(|| anyhow!("no sessions recorded for this workspace"))?;
    let session = state
        .sessions
        .iter_mut()
        .find(|s| s.name == name)
        .ok_or_else(|| anyhow!("no such session: \"{name}\""))?;
    let result = edit(session);
    state.updated_at = Utc::now();
    store.save(&state)?;
    Ok(result)
}

/// Set (or clear) a session's sidebar display name override in `state.json`,
/// leaving its branch / identity untouched.
///
/// `display` is trimmed; an empty string — or one equal to the session name —
/// clears the override. Returns the override now stored: `Some(name)` when a
/// distinct display name is set, or `None` when cleared (i.e. the session falls
/// back to its branch name). Resolving that into the label actually shown is the
/// presentation layer's job, so this usecase persists the raw value and does not
/// decide the displayed string. Fails when no session named `name` exists.
pub fn set_display_name(
    workspace_root: &Path,
    name: &str,
    display: &str,
) -> Result<Option<String>> {
    let store = WorkspaceStore::new(workspace_root);
    edit_session(&store, name, |session| {
        let trimmed = display.trim();
        session.display_name = if trimmed.is_empty() || trimmed == session.name {
            None
        } else {
            Some(trimmed.to_string())
        };
        session.display_name.clone()
    })
}

/// Set (or clear) a session's free-form note in `state.json`, leaving its branch
/// / identity untouched.
///
/// The note is stored as written (multi-line text is kept verbatim) save for
/// trailing whitespace / blank lines, which are trimmed; a note that trims to
/// empty clears it, so the session has no note again. Returns the note now
/// stored (`None` when cleared). Fails when no session named `name` exists.
pub fn set_note(workspace_root: &Path, name: &str, note: &str) -> Result<Option<String>> {
    let store = WorkspaceStore::new(workspace_root);
    edit_session(&store, name, |session| {
        let trimmed = note.trim_end();
        session.note = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
        session.note.clone()
    })
}

/// Return a session's free-form note, or `None` when none has been written.
/// Fails when no session named `name` exists.
pub fn get_note(workspace_root: &Path, name: &str) -> Result<Option<String>> {
    let store = WorkspaceStore::new(workspace_root);
    let state = store
        .load()?
        .ok_or_else(|| anyhow!("no sessions recorded for this workspace"))?;
    state
        .sessions
        .into_iter()
        .find(|s| s.name == name)
        .map(|s| s.note)
        .ok_or_else(|| anyhow!("no such session: \"{name}\""))
}

/// Set (or clear) the workspace **root**'s free-form note in `state.json` — the
/// `⌂ root` row's counterpart to [`set_note`], which targets a session.
///
/// The note is trimmed and cleared-when-empty exactly as [`set_note`] handles a
/// session's, and returns the note now stored (`None` when cleared). Unlike
/// [`set_note`] this never errors on a missing `state.json`: the root belongs to
/// no session, so a workspace with no sessions recorded yet can still carry a
/// root note — the state is created (defaulted) when absent. Takes the same store
/// lock across the read-modify-write so it serialises against concurrent writers.
pub fn set_root_note(workspace_root: &Path, note: &str) -> Result<Option<String>> {
    let store = WorkspaceStore::new(workspace_root);
    let _lock = store.lock()?;
    let mut state = store.load()?.unwrap_or_default();
    let trimmed = note.trim_end();
    state.root_note = if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    };
    let stored = state.root_note.clone();
    state.updated_at = Utc::now();
    store.save(&state)?;
    Ok(stored)
}

/// List the sessions recorded for `workspace_root`, in stored order.
///
/// The `sessions` array order *is* the display order shown in the home list —
/// initially creation order, then whatever the user has reordered it to (see
/// [`reorder`]). Returns an empty list when no state has been written yet (a
/// workspace with no sessions). This reads `state.json` only — it does not
/// reconcile the on-disk tree, so it is a cheap query callers can run freely.
pub fn list(workspace_root: &Path) -> Result<Vec<SessionRecord>> {
    let store = WorkspaceStore::new(workspace_root);
    Ok(store
        .load()?
        .map(|state| state.sessions)
        .unwrap_or_default())
}

/// Return the pull requests associated with session `name`, along with the
/// session's root worktree.
///
/// PR links are primarily persisted in the out-of-band [`pr_link_store`] as soon
/// as an agent prints a pull-request URL; workspace sync later folds them into
/// `state.json` so the TUI can show badges from saved state. Read both sources
/// here and de-duplicate by URL so MCP callers see the newest harvested links
/// even before the next sync, while still preserving links already present in
/// older state files.
pub fn pr_links(workspace_root: &Path, name: &str) -> Result<(PathBuf, Vec<PrLink>)> {
    let session = list(workspace_root)?
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| anyhow!("no such session: \"{name}\""))?;
    let state_prs = session
        .worktrees
        .iter()
        .flat_map(|wt| wt.pr.iter().cloned());
    let recorded_prs = pr_link_store::get(&session.root);
    Ok((
        session.root,
        PrLink::aggregate(state_prs.chain(recorded_prs)),
    ))
}

/// Move session `name` one row toward the top (`up = true`) or bottom of the
/// recorded order in `state.json`, returning whether the order changed.
///
/// The `sessions` array order is the home list's display order, so reordering
/// is a swap of adjacent entries persisted in place — there is no separate
/// order field to keep in sync. Moving the first session up, or the last one
/// down, is a no-op that leaves `state.json` untouched and returns `false`; an
/// unknown `name` errors.
pub fn reorder(workspace_root: &Path, name: &str, up: bool) -> Result<bool> {
    let store = WorkspaceStore::new(workspace_root);
    // Hold the lock across the load → edit → save so a concurrent writer cannot
    // overwrite this reorder (or have it overwrite their change), matching
    // [`set_display_name`] / [`set_note`].
    let _lock = store.lock()?;
    let mut state = store
        .load()?
        .ok_or_else(|| anyhow!("no sessions recorded for this workspace"))?;
    let index = state
        .sessions
        .iter()
        .position(|s| s.name == name)
        .ok_or_else(|| anyhow!("no such session: \"{name}\""))?;
    let target = if up {
        index.checked_sub(1)
    } else {
        Some(index + 1).filter(|&t| t < state.sessions.len())
    };
    let Some(target) = target else {
        return Ok(false);
    };
    state.sessions.swap(index, target);
    state.updated_at = Utc::now();
    store.save(&state)?;
    Ok(true)
}

/// Persist the in-memory `last_active` timestamps the home screen accumulates
/// while running, merging them into `state.json` in one write.
///
/// The sidebar's freshness ("heat") dot is bumped in memory on every session
/// switch and burst of terminal/agent activity, so persisting on each of those
/// would hammer the store on a hot path. Instead the home screen flushes the
/// collected `(name, last_active)` pairs once — on quit — through here. Each pair
/// updates the matching session's [`last_active`](SessionRecord::last_active);
/// names with no matching session are ignored. Returns `false` (and writes
/// nothing) when there is no state, no pairs, or none of them change a value, so
/// a quit with no activity leaves `state.json` untouched.
pub fn persist_last_active(
    workspace_root: &Path,
    actives: &[(String, DateTime<Utc>)],
) -> Result<bool> {
    if actives.is_empty() {
        return Ok(false);
    }
    let store = WorkspaceStore::new(workspace_root);
    let _lock = store.lock()?;
    let Some(mut state) = store.load()? else {
        return Ok(false);
    };
    let mut changed = false;
    for session in &mut state.sessions {
        if let Some((_, ts)) = actives.iter().find(|(name, _)| name == &session.name) {
            if session.last_active != Some(*ts) {
                session.last_active = Some(*ts);
                changed = true;
            }
        }
    }
    if !changed {
        return Ok(false);
    }
    state.updated_at = Utc::now();
    store.save(&state)?;
    Ok(true)
}

/// The result of attempting to remove a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalOutcome {
    /// `true` when the session was removed; `false` when blocked by `dirty`.
    pub removed: bool,
    /// Worktrees with uncommitted changes that blocked a non-forced removal.
    /// Empty when the session was removed.
    pub dirty: Vec<PathBuf>,
}

/// Remove session `name` under `workspace_root`: delete every repository's
/// worktree and session branch, drop any copied files, clear each worktree's
/// agent chat history and running-state, and forget it in `state.json`.
///
/// `agent` is the session's configured agent CLI: its persisted conversation for
/// each worktree (e.g. Claude's transcript directory) is discarded so the chat
/// history does not outlive the session, and a session recreated at the same
/// path later starts fresh. usagi's own per-worktree files keyed by the worktree
/// path are cleared too — the agent phase ([`agent_state_store`]), the discovered
/// PR badges ([`pr_link_store`]), any queued prompt ([`agent_prompt_store`]), and
/// the open-pane snapshot ([`open_panes_store`]) — so none of them is re-read by a
/// session later recreated at the same path.
///
/// Without `force`, a session whose worktrees have uncommitted changes is left
/// untouched and the dirty worktrees are returned for the caller to warn about.
/// With `force`, those changes are discarded.
pub fn remove(
    workspace_root: &Path,
    name: &str,
    force: bool,
    agent: &dyn Agent,
) -> Result<RemovalOutcome> {
    let store = WorkspaceStore::new(workspace_root);
    // Hold the lock across the whole operation — reconcile → drop-the-record →
    // save — so a concurrent writer cannot resurrect the removed session or lose
    // an unrelated change, and reconcile's load-and-destroy cannot race a
    // concurrent create that is mid-build.
    let _lock = store.lock()?;

    // Sync the on-disk tree with the recorded sessions: any session directory
    // `state.json` does not know about is force-removed regardless of
    // uncommitted changes (the recorded `name` itself keeps its dirty guard).
    reconcile::reconcile_locked(workspace_root)?;

    let mut state = store
        .load()?
        .ok_or_else(|| anyhow!("no sessions recorded for this workspace"))?;
    let index = state
        .sessions
        .iter()
        .position(|s| s.name == name)
        .ok_or_else(|| anyhow!("no such session: \"{name}\""))?;
    // Take the record out of the in-memory state rather than cloning it: on the
    // dirty early-return below we never save, so the on-disk state is untouched,
    // and on the success path the state already has the session dropped by the
    // time it is saved (no second `remove`).
    let session = state.sessions.remove(index);

    // Refuse to discard uncommitted work unless forced. Dirtiness goes through
    // the same single `worktree_status` call the rest of the codebase uses; a
    // path that is not (or no longer) a git worktree reports clean.
    let dirty: Vec<PathBuf> = session
        .worktrees
        .iter()
        .filter(|wt| git::worktree_status(&wt.path).is_some_and(|s| s.dirty))
        .map(|wt| wt.path.clone())
        .collect();
    if !dirty.is_empty() && !force {
        return Ok(RemovalOutcome {
            removed: false,
            dirty,
        });
    }

    // Clear the chat history and every per-worktree file usagi keeps for each
    // worktree so nothing outlives the session: a path reused later starts clean
    // rather than inheriting the removed session's agent phase, PR badges, queued
    // prompt, or open-pane snapshot — all of which are keyed by the worktree path
    // and would otherwise be re-read by a session recreated there. The TUI also
    // clears the phase and pane snapshot as it evicts the live pool, but removal
    // can come from the CLI or MCP with no TUI running, so the durable files are
    // wiped here for every caller. This runs *before* the worktree directories
    // are removed, so the canonicalized worktree path still resolves to the key
    // the running agent recorded under.
    for wt in &session.worktrees {
        agent.forget_session(&wt.path);
        agent_state_store::clear(&wt.path);
        pr_link_store::clear(&wt.path);
        agent_prompt_store::clear(&wt.path);
        open_panes_store::clear(&wt.path);
    }

    // Physically destroy the session: unregister each repository's worktree on
    // the session branch, drop the branch, and delete the session tree. This is
    // the same primitive `reconcile` uses to prune strays — located via
    // `list_worktrees` rather than the recorded paths, which also tolerates a
    // ghost session whose worktree was never built (nothing matches the branch,
    // so git is left untouched and only the record is dropped below).
    let repo_worktrees = reconcile::list_repo_worktrees(workspace_root)?;
    reconcile::discard_session(&session.root, &branch_name(name), &repo_worktrees, force)?;

    state.updated_at = Utc::now();
    store.save(&state)?;

    crate::infrastructure::trace_log::TraceLog::record(
        crate::domain::trace::TraceEvent::now(
            crate::domain::trace::TraceCategory::Session,
            "remove",
        )
        .with_detail(name),
    );

    Ok(RemovalOutcome {
        removed: true,
        dirty: Vec::new(),
    })
}

/// Resolve the workspace root from a working directory that may sit inside a
/// session tree.
///
/// A session is mirrored at `<workspace>/.usagi/sessions/<name>/...`. When a
/// process runs from within such a tree (e.g. an agent's `usagi mcp` server),
/// session orchestration still operates on the whole *workspace* — the session
/// registry and every sibling worktree live under `<workspace>/.usagi/`, not in
/// the throwaway copy under the current session that `usagi clean` later
/// deletes. So we strip everything from the `.usagi/sessions` segment onward and
/// return the workspace root. A path that is not inside a session tree is
/// returned unchanged.
///
/// Issues and memories, by contrast, are resolved against the *current* worktree
/// (see [`crate::presentation::cli::mcp`]) so a session's edits land on its own
/// branch and reach `main` through its PR. Issue numbering still consults every
/// worktree via [`session_roots`] to stay collision-free across the workspace.
pub fn workspace_root(start: &Path) -> PathBuf {
    let mut prefix = PathBuf::new();
    let mut components = start.components().peekable();
    while let Some(component) = components.next() {
        if component.as_os_str() == OsStr::new(STATE_DIR)
            && components
                .peek()
                .is_some_and(|next| next.as_os_str() == OsStr::new(SESSIONS_DIR))
        {
            return prefix;
        }
        prefix.push(component);
    }
    start.to_path_buf()
}

/// The source git repositories a session under `workspace_root` spans: the root
/// itself when it is a repository, otherwise every repository reached by the
/// recursive workspace walk.
///
/// This is the set whose default branches `usagi update` refreshes from the
/// remote — the same repositories a new session cuts a worktree in — so the two
/// views of "which repos does this workspace contain" stay in sync. Returns an
/// empty list for a non-git, repo-less root.
pub fn source_repos(workspace_root: &Path) -> Vec<PathBuf> {
    tree::source_repos(workspace_root)
}

/// Every existing session worktree root under `<workspace_root>/.usagi/sessions/`.
///
/// Each entry is `<workspace_root>/.usagi/sessions/<name>`. Returns an empty vec
/// when the sessions directory does not exist. Used by issue numbering to scan
/// every session's own issue store for the workspace-wide maximum, so two
/// sessions never mint the same number.
pub fn session_roots(workspace_root: &Path) -> Vec<PathBuf> {
    let dir = workspace_root.join(STATE_DIR).join(SESSIONS_DIR);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::git::test_command as git_cmd;
    use crate::infrastructure::setup_runner::SystemSetupCommandRunner;
    use crate::usecase::settings;
    use anyhow::anyhow;
    use std::cell::RefCell;

    /// Initialise a throwaway git repo with one commit on `main`.
    fn init_repo(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        let run = |args: &[&str]| {
            assert!(
                git_cmd(dir).args(args).status().unwrap().success(),
                "git {args:?} failed"
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@e.com"]);
        run(&["config", "user.name", "t"]);
        fs::write(dir.join("code.txt"), "x").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
    }

    /// The branch checked out in the worktree at `dir`.
    fn head_branch(dir: &Path) -> String {
        let out = git_cmd(dir)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// The full HEAD commit at the worktree `dir`.
    fn head_commit(dir: &Path) -> String {
        let out = git_cmd(dir).args(["rev-parse", "HEAD"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[derive(Default)]
    struct RecordingSetupRunner {
        calls: RefCell<Vec<(PathBuf, String)>>,
        fail_on: Option<String>,
    }

    impl SetupCommandRunner for RecordingSetupRunner {
        fn run(&self, cwd: &Path, command: &str) -> Result<()> {
            self.calls
                .borrow_mut()
                .push((cwd.to_path_buf(), command.to_string()));
            if self.fail_on.as_deref() == Some(command) {
                Err(anyhow!("boom"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn system_setup_runner_runs_in_the_given_directory_and_reports_failure() {
        let dir = tempfile::tempdir().unwrap();
        let runner = SystemSetupCommandRunner;
        #[cfg(not(windows))]
        let command = "printf hello > setup.txt";
        #[cfg(windows)]
        let command = "echo hello> setup.txt";

        runner.run(dir.path(), command).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("setup.txt")).unwrap(),
            "hello"
        );

        // Failing command with non-empty stderr (covers the stderr branch).
        #[cfg(not(windows))]
        let failing = "echo nope >&2; exit 7";
        #[cfg(windows)]
        let failing = "exit /B 7";
        let err = runner.run(dir.path(), failing).unwrap_err();
        assert!(err.to_string().contains("setup command"));

        // Failing command with non-empty stdout and empty stderr (covers the
        // stdout branch and the empty-stderr branch in the error message).
        #[cfg(not(windows))]
        {
            let err = runner
                .run(dir.path(), "echo hi; exit 2")
                .unwrap_err()
                .to_string();
            assert!(err.contains("stdout"));
            assert!(!err.contains("stderr"));
        }
    }

    #[test]
    fn rejects_an_empty_name() {
        let dir = tempfile::tempdir().unwrap();
        let err = create(dir.path(), "   ").unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn rejects_a_name_with_path_separators() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["a/b", "a\\b", ".", ".."] {
            let err = create(dir.path(), bad).unwrap_err();
            assert!(err.to_string().contains("must not contain path separators"));
        }
    }

    #[test]
    fn rejects_a_name_starting_with_a_dash() {
        // A leading-`-` name would be interpolated into git commands as a branch
        // operand and parsed as an option (`git branch -D -D`), so it is refused
        // up front before any repository is touched.
        let dir = tempfile::tempdir().unwrap();
        for bad in ["-D", "--foo", "-"] {
            let err = create(dir.path(), bad).unwrap_err();
            assert!(
                err.to_string().contains("must not start with"),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn single_repo_root_gets_one_worktree() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());

        let created = create(root.path(), "feature-x").unwrap();

        let wt = root.path().join(".usagi/sessions/feature-x");
        assert_eq!(created.root, wt);
        assert_eq!(created.worktrees, vec![wt.clone()]);
        // The new worktree is on the namespaced session branch and carries the
        // repo files.
        assert_eq!(head_branch(&wt), "usagi/feature-x");
        assert!(wt.join("code.txt").is_file());
        // The session is recorded in state.json.
        let state = WorkspaceStore::new(root.path()).load().unwrap().unwrap();
        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[0].name, "feature-x");
        assert_eq!(state.sessions[0].root, wt);
    }

    #[test]
    fn create_runs_configured_setup_commands_in_the_session_root() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        settings::save_local(
            root.path(),
            &LocalSettings {
                setup_commands: vec!["first".to_string(), "  ".to_string(), "second".to_string()],
                ..Default::default()
            },
        )
        .unwrap();
        let runner = RecordingSetupRunner::default();

        let created = create_with_setup_runner(root.path(), "with-setup", &runner).unwrap();

        assert_eq!(
            *runner.calls.borrow(),
            vec![
                (created.root.clone(), "first".to_string()),
                (created.root.clone(), "second".to_string()),
            ]
        );
        assert_eq!(list(root.path()).unwrap()[0].name, "with-setup");
    }

    #[test]
    fn setup_command_failures_are_logged_without_aborting_creation() {
        let _guard = crate::test_support::process_env_guard();
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, data.path());
        init_repo(root.path());
        settings::save_local(
            root.path(),
            &LocalSettings {
                setup_commands: vec!["fail".to_string(), "after".to_string()],
                ..Default::default()
            },
        )
        .unwrap();
        let runner = RecordingSetupRunner {
            fail_on: Some("fail".to_string()),
            ..Default::default()
        };

        let created = create_with_setup_runner(root.path(), "keeps-going", &runner).unwrap();

        assert!(created.root.exists());
        assert_eq!(
            runner
                .calls
                .borrow()
                .iter()
                .map(|(_, command)| command.as_str())
                .collect::<Vec<_>>(),
            vec!["fail", "after"]
        );
        assert!(list(root.path())
            .unwrap()
            .iter()
            .any(|session| session.name == "keeps-going"));
        assert!(data.path().join("logs").is_dir());
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn non_git_root_recurses_over_repos_and_copies_files() {
        let root = tempfile::tempdir().unwrap();
        // Two top-level repos, a plain nested dir holding a third repo, and a
        // loose file at the root — mirroring the multi-repo example.
        init_repo(&root.path().join("app-a"));
        init_repo(&root.path().join("app-b"));
        init_repo(&root.path().join("be/be1"));
        fs::write(root.path().join("README.md"), "hi").unwrap();
        // A pre-existing .usagi dir must be skipped, not copied into the session.
        fs::create_dir_all(root.path().join(".usagi")).unwrap();
        fs::write(root.path().join(".usagi/marker"), "x").unwrap();

        let created = create(root.path(), "wip").unwrap();

        let base = root.path().join(".usagi/sessions/wip");
        // Every repository became a worktree on the session branch.
        for repo in ["app-a", "app-b", "be/be1"] {
            let wt = base.join(repo);
            assert!(wt.is_dir(), "{repo} worktree missing");
            assert_eq!(head_branch(&wt), "usagi/wip");
            assert!(created.worktrees.contains(&wt));
        }
        assert_eq!(created.worktrees.len(), 3);
        // The loose file was copied; usagi's own data dir was not.
        assert_eq!(fs::read_to_string(base.join("README.md")).unwrap(), "hi");
        assert!(!base.join(".usagi").exists());
        // The session is recorded even though the root is not a git repository.
        let state = WorkspaceStore::new(root.path()).load().unwrap().unwrap();
        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[0].worktrees.len(), 3);
    }

    /// Add a linked worktree of `repo` at `dest` on a throwaway branch; its
    /// `.git` is a file pointer, marking it as an existing worktree to skip.
    fn add_linked_worktree(repo: &Path, dest: &Path, branch: &str) {
        assert!(git_cmd(repo)
            .args([
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                dest.to_str().unwrap()
            ])
            .status()
            .unwrap()
            .success());
        assert!(dest.join(".git").is_file());
    }

    #[test]
    fn create_skips_existing_linked_worktrees() {
        let root = tempfile::tempdir().unwrap();
        // A real repo at the root is mirrored, but a linked worktree sitting
        // alongside it (e.g. a `.workspace` or `.claude/worktrees/*`) is left
        // untouched: not branched, not copied into the session.
        init_repo(&root.path().join("app"));
        add_linked_worktree(
            &root.path().join("app"),
            &root.path().join(".workspace"),
            "wt",
        );

        let created = create(root.path(), "wip").unwrap();

        let base = root.path().join(".usagi/sessions/wip");
        assert_eq!(created.worktrees, vec![base.join("app")]);
        assert!(!base.join(".workspace").exists());
    }

    #[test]
    fn source_repos_skips_linked_worktrees() {
        let root = tempfile::tempdir().unwrap();
        init_repo(&root.path().join("app"));
        add_linked_worktree(
            &root.path().join("app"),
            &root.path().join(".workspace"),
            "wt",
        );

        // Only the real repository is a source repo; the linked worktree is not.
        let repos = tree::source_repos(root.path());
        assert_eq!(repos, vec![root.path().join("app")]);
    }

    #[test]
    fn records_multiple_sessions_in_order() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());

        create(root.path(), "first").unwrap();
        // The second create loads the existing state and appends to it.
        create(root.path(), "second").unwrap();

        let state = WorkspaceStore::new(root.path()).load().unwrap().unwrap();
        let names: Vec<&str> = state.sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["first", "second"]);
    }

    #[test]
    fn rejects_a_duplicate_session() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        create(root.path(), "dup").unwrap();

        let err = create(root.path(), "dup").unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn surfaces_a_git_error_when_the_branch_exists() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        // Pre-create the *namespaced* branch the session would cut so
        // `git worktree add -b usagi/taken` fails. A plain `taken` branch would
        // not collide — sessions live under `usagi/`.
        assert!(git_cmd(root.path())
            .args(["branch", "usagi/taken"])
            .status()
            .unwrap()
            .success());

        let err = create(root.path(), "taken").unwrap_err();
        assert!(err.to_string().contains("git worktree add failed"));
    }

    #[test]
    fn a_plain_branch_sharing_the_session_name_does_not_collide() {
        // The whole point of the `usagi/` namespace: a hand-made branch named
        // exactly like the session no longer blocks creating it.
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        assert!(git_cmd(root.path())
            .args(["branch", "feature"])
            .status()
            .unwrap()
            .success());

        let created = create(root.path(), "feature").unwrap();
        assert_eq!(head_branch(&created.root), "usagi/feature");
        // Both branches coexist: the user's `feature` and the session's.
        assert!(branch_exists(root.path(), "feature"));
        assert!(branch_exists(root.path(), "usagi/feature"));
    }

    #[test]
    fn existing_branch_names_unions_local_branches_across_repos() {
        // A multi-repo workspace: each repo's local branches are unioned, sorted
        // and de-duplicated; remote-tracking refs are excluded.
        let root = tempfile::tempdir().unwrap();
        init_repo(&root.path().join("app-a"));
        init_repo(&root.path().join("be/be1"));
        let run = |dir: &Path, args: &[&str]| {
            assert!(git_cmd(dir).args(args).status().unwrap().success());
        };
        run(&root.path().join("app-a"), &["branch", "test/x"]);
        run(&root.path().join("be/be1"), &["branch", "feature"]);

        let names = existing_branch_names(root.path());
        // Both repos start on `main` (deduped) plus each one's extra branch.
        assert_eq!(
            names,
            vec![
                "feature".to_string(),
                "main".to_string(),
                "test/x".to_string()
            ]
        );

        // A non-git, empty root contributes nothing.
        let empty = tempfile::tempdir().unwrap();
        assert!(existing_branch_names(empty.path()).is_empty());
    }

    #[test]
    fn rejects_a_name_that_clashes_with_a_branch_namespace() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        // Pre-create branches nested under the session's namespaced branch
        // `usagi/test/…`. The `usagi/test` branch then cannot be created.
        for branch in ["usagi/test/home-ui-e2e", "usagi/test/tui-e2e-pty"] {
            assert!(git_cmd(root.path())
                .args(["branch", branch])
                .status()
                .unwrap()
                .success());
        }

        let err = create(root.path(), "test").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("conflicts with the existing branch"), "{msg}");
        assert!(msg.contains("usagi/test/home-ui-e2e"), "{msg}");
        // Nothing was created on the failed attempt.
        assert!(!root.path().join(".usagi/sessions/test").exists());
        assert!(sessions_of(root.path()).is_empty());
    }

    #[test]
    fn branches_from_remote_by_default_and_from_local_when_configured() {
        use crate::domain::settings::{BranchSource, LocalSettings};

        // A repo whose local `main` is one commit ahead of `origin/main`, so the
        // two refs resolve to different commits and the chosen base is provable.
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("remote.git");
        let root = tmp.path().join("work");
        let run = |dir: &Path, args: &[&str]| {
            assert!(git_cmd(dir).args(args).status().unwrap().success());
        };

        // `-b main` keeps the bare repo's HEAD on `main`, matching the other
        // test remotes so the idiom is consistent and host-`init.defaultBranch`-proof.
        run(
            tmp.path(),
            &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
        );
        init_repo(&root);
        run(&root, &["remote", "add", "origin", bare.to_str().unwrap()]);
        run(&root, &["push", "-q", "-u", "origin", "main"]);
        run(&root, &["remote", "set-head", "origin", "main"]);
        let remote_commit = head_commit(&root); // origin/main == first commit
                                                // Advance local main ahead of the remote.
        fs::write(root.join("code.txt"), "second").unwrap();
        run(&root, &["commit", "-aqm", "second"]);
        let local_commit = head_commit(&root);
        assert_ne!(remote_commit, local_commit);

        // Default (no local settings): session branches from origin/main.
        let created = create(&root, "from-remote").unwrap();
        assert_eq!(head_commit(&created.root), remote_commit);

        // Configured Local: session branches from the local default branch.
        settings::save_local(
            &root,
            &LocalSettings {
                default_branch_source: Some(BranchSource::Local),
                ..Default::default()
            },
        )
        .unwrap();
        let created = create(&root, "from-local").unwrap();
        assert_eq!(head_commit(&created.root), local_commit);
    }

    #[test]
    fn branches_from_a_configured_specific_branch() {
        use crate::domain::settings::LocalSettings;

        // A repo whose `develop` branch sits at a different commit than `main`,
        // so the chosen base is provable from the resulting HEAD.
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let main_commit = head_commit(root.path());
        let run = |args: &[&str]| {
            assert!(git_cmd(root.path()).args(args).status().unwrap().success());
        };
        run(&["checkout", "-q", "-b", "develop"]);
        fs::write(root.path().join("code.txt"), "on develop").unwrap();
        run(&["commit", "-aqm", "develop work"]);
        let develop_commit = head_commit(root.path());
        run(&["checkout", "-q", "main"]);
        assert_ne!(main_commit, develop_commit);

        // Configure the session base to the `develop` branch (local form).
        settings::save_local(
            root.path(),
            &LocalSettings {
                default_branch_source: Some(crate::domain::settings::BranchSource::Local),
                default_branch: Some("develop".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        let created = create(root.path(), "from-develop").unwrap();
        assert_eq!(head_commit(&created.root), develop_commit);
    }

    #[test]
    fn fails_when_the_session_directory_cannot_be_created() {
        let root = tempfile::tempdir().unwrap();
        // A non-repo root whose `.usagi` is a *file* makes create_dir_all fail.
        fs::write(root.path().join(".usagi"), "not a dir").unwrap();

        let err = create(root.path(), "x").unwrap_err();
        assert!(err.to_string().contains("failed to create"));
    }

    // --- list --------------------------------------------------------------

    #[test]
    fn list_returns_recorded_sessions_in_order() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        // No state yet: an empty list, not an error.
        assert!(list(root.path()).unwrap().is_empty());

        create(root.path(), "first").unwrap();
        create(root.path(), "second").unwrap();

        let names: Vec<String> = list(root.path())
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["first", "second"]);
    }

    // --- reorder -----------------------------------------------------------

    fn ordered_names(root: &Path) -> Vec<String> {
        list(root).unwrap().into_iter().map(|s| s.name).collect()
    }

    #[test]
    fn reorder_moves_a_session_up_and_down_and_clamps_at_the_ends() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        create(root.path(), "a").unwrap();
        create(root.path(), "b").unwrap();
        create(root.path(), "c").unwrap();
        assert_eq!(ordered_names(root.path()), vec!["a", "b", "c"]);

        // Up swaps with the previous neighbour.
        assert!(reorder(root.path(), "b", true).unwrap());
        assert_eq!(ordered_names(root.path()), vec!["b", "a", "c"]);

        // Down swaps with the next neighbour.
        assert!(reorder(root.path(), "a", false).unwrap());
        assert_eq!(ordered_names(root.path()), vec!["b", "c", "a"]);

        // The first session up and the last down are no-ops that report no change
        // and leave the order untouched.
        assert!(!reorder(root.path(), "b", true).unwrap());
        assert!(!reorder(root.path(), "a", false).unwrap());
        assert_eq!(ordered_names(root.path()), vec!["b", "c", "a"]);
    }

    #[test]
    fn reorder_errors_without_state_or_session() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        // No state.json yet.
        let err = reorder(root.path(), "x", true).unwrap_err();
        assert!(err.to_string().contains("no sessions recorded"));

        // State exists but the named session does not.
        create(root.path(), "present").unwrap();
        let err = reorder(root.path(), "absent", true).unwrap_err();
        assert!(err.to_string().contains("no such session"));
    }

    // --- persist_last_active ----------------------------------------------

    #[test]
    fn persist_last_active_merges_timestamps_skipping_unknown_and_unchanged() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());

        // No state yet, and an empty input, both write nothing.
        assert!(!persist_last_active(root.path(), &[("x".to_string(), Utc::now())]).unwrap());
        create(root.path(), "a").unwrap();
        create(root.path(), "b").unwrap();
        assert!(!persist_last_active(root.path(), &[]).unwrap());

        // Stamps the matching session and ignores an unknown name.
        let ts = Utc::now();
        assert!(persist_last_active(
            root.path(),
            &[("a".to_string(), ts), ("ghost".to_string(), ts)],
        )
        .unwrap());
        let sessions = list(root.path()).unwrap();
        let a = sessions.iter().find(|s| s.name == "a").unwrap();
        let b = sessions.iter().find(|s| s.name == "b").unwrap();
        assert_eq!(a.last_active, Some(ts));
        assert_eq!(b.last_active, None);

        // Re-applying the same value changes nothing, so no write happens.
        assert!(!persist_last_active(root.path(), &[("a".to_string(), ts)]).unwrap());
    }

    // --- set_display_name --------------------------------------------------

    fn display_name_of(root: &Path, name: &str) -> Option<String> {
        list(root)
            .unwrap()
            .into_iter()
            .find(|s| s.name == name)
            .and_then(|s| s.display_name)
    }

    #[test]
    fn set_display_name_sets_clears_and_leaves_other_sessions_alone() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        create(root.path(), "feature").unwrap();
        create(root.path(), "other").unwrap();

        // Set an override → it is stored and returned as the raw override value.
        let stored = set_display_name(root.path(), "feature", "Nice name").unwrap();
        assert_eq!(stored.as_deref(), Some("Nice name"));
        assert_eq!(
            display_name_of(root.path(), "feature").as_deref(),
            Some("Nice name")
        );
        // The branch / identity is untouched and other sessions keep their state.
        assert_eq!(display_name_of(root.path(), "other"), None);

        // A surrounding-whitespace label is trimmed before storing.
        set_display_name(root.path(), "feature", "  Spaced  ").unwrap();
        assert_eq!(
            display_name_of(root.path(), "feature").as_deref(),
            Some("Spaced")
        );

        // An empty label clears the override → the raw stored value is None (the
        // sidebar falls back to the session name, but that resolution is the
        // presentation layer's, not this usecase's).
        let stored = set_display_name(root.path(), "feature", "   ").unwrap();
        assert_eq!(stored, None);
        assert_eq!(display_name_of(root.path(), "feature"), None);

        // A label equal to the session name is treated as "no override".
        set_display_name(root.path(), "feature", "feature").unwrap();
        assert_eq!(display_name_of(root.path(), "feature"), None);
    }

    fn note_of(root: &Path, name: &str) -> Option<String> {
        list(root)
            .unwrap()
            .into_iter()
            .find(|s| s.name == name)
            .and_then(|s| s.note)
    }

    #[test]
    fn set_note_sets_trims_clears_and_leaves_other_sessions_alone() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        create(root.path(), "feature").unwrap();
        create(root.path(), "other").unwrap();

        // A multi-line note is stored verbatim and returned.
        let stored = set_note(root.path(), "feature", "line 1\nline 2").unwrap();
        assert_eq!(stored.as_deref(), Some("line 1\nline 2"));
        assert_eq!(
            note_of(root.path(), "feature").as_deref(),
            Some("line 1\nline 2")
        );
        // The other session is untouched.
        assert_eq!(note_of(root.path(), "other"), None);

        // Trailing whitespace / blank lines are trimmed off the end.
        let stored = set_note(root.path(), "feature", "kept\n\n   \n").unwrap();
        assert_eq!(stored.as_deref(), Some("kept"));

        // A note that trims to empty clears it.
        let stored = set_note(root.path(), "feature", "   \n  ").unwrap();
        assert_eq!(stored, None);
        assert_eq!(note_of(root.path(), "feature"), None);
    }

    #[test]
    fn get_note_returns_the_stored_note_and_errors_without_state_or_session() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        // No state.json yet.
        let err = get_note(root.path(), "x").unwrap_err();
        assert!(err.to_string().contains("no sessions recorded"));

        // State exists but the named session does not.
        create(root.path(), "present").unwrap();
        let err = get_note(root.path(), "absent").unwrap_err();
        assert!(err.to_string().contains("no such session"));

        // Session exists with no note → Ok(None).
        assert_eq!(get_note(root.path(), "present").unwrap(), None);

        // After setting a note it is returned.
        set_note(root.path(), "present", "hello").unwrap();
        assert_eq!(
            get_note(root.path(), "present").unwrap().as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn set_note_errors_without_state_or_session() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        // No state.json yet.
        let err = set_note(root.path(), "x", "hi").unwrap_err();
        assert!(err.to_string().contains("no sessions recorded"));

        // State exists but the named session does not.
        create(root.path(), "present").unwrap();
        let err = set_note(root.path(), "absent", "hi").unwrap_err();
        assert!(err.to_string().contains("no such session"));
    }

    fn root_note_of(root: &Path) -> Option<String> {
        WorkspaceStore::new(root)
            .load()
            .unwrap()
            .and_then(|s| s.root_note)
    }

    #[test]
    fn set_root_note_sets_trims_and_clears_without_a_session() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        // No state.json yet: unlike `set_note`, the root note can be written before
        // any session exists — the state is created on demand.
        let stored = set_root_note(root.path(), "root line 1\nroot line 2").unwrap();
        assert_eq!(stored.as_deref(), Some("root line 1\nroot line 2"));
        assert_eq!(
            root_note_of(root.path()).as_deref(),
            Some("root line 1\nroot line 2")
        );

        // Trailing whitespace / blank lines are trimmed off the end.
        let stored = set_root_note(root.path(), "kept\n\n  \n").unwrap();
        assert_eq!(stored.as_deref(), Some("kept"));

        // A note that trims to empty clears it.
        let stored = set_root_note(root.path(), "   \n ").unwrap();
        assert_eq!(stored, None);
        assert_eq!(root_note_of(root.path()), None);
    }

    #[test]
    fn set_root_note_leaves_sessions_untouched() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        create(root.path(), "feature").unwrap();
        set_note(root.path(), "feature", "session memo").unwrap();

        set_root_note(root.path(), "root memo").unwrap();
        // The root note is recorded alongside the session, which keeps its own note.
        assert_eq!(root_note_of(root.path()).as_deref(), Some("root memo"));
        assert_eq!(
            note_of(root.path(), "feature").as_deref(),
            Some("session memo")
        );
        assert_eq!(
            list(root.path())
                .unwrap()
                .into_iter()
                .map(|s| s.name)
                .collect::<Vec<_>>(),
            vec!["feature".to_string()]
        );
    }

    #[test]
    fn set_display_name_errors_without_state_or_session() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        // No state.json yet.
        let err = set_display_name(root.path(), "x", "label").unwrap_err();
        assert!(err.to_string().contains("no sessions recorded"));

        // State exists but the named session does not.
        create(root.path(), "present").unwrap();
        let err = set_display_name(root.path(), "absent", "label").unwrap_err();
        assert!(err.to_string().contains("no such session"));
    }

    // --- remove ------------------------------------------------------------

    fn sessions_of(root: &Path) -> Vec<String> {
        WorkspaceStore::new(root)
            .load()
            .unwrap()
            .map(|s| s.sessions.into_iter().map(|r| r.name).collect())
            .unwrap_or_default()
    }

    /// A throwaway agent for the `remove` tests. Gemini keeps no conversation
    /// store, so its `forget_session` is a no-op — removal touches no real files
    /// outside the workspace.
    fn noop_agent() -> std::sync::Arc<dyn crate::domain::agent::Agent> {
        crate::infrastructure::agent::agent_for(crate::domain::settings::AgentCli::Gemini)
    }

    #[test]
    fn remove_errors_without_state_or_session() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        // No state.json yet.
        let err = remove(root.path(), "x", false, noop_agent().as_ref()).unwrap_err();
        assert!(err.to_string().contains("no sessions recorded"));

        // State exists but the named session does not.
        create(root.path(), "present").unwrap();
        let err = remove(root.path(), "absent", false, noop_agent().as_ref()).unwrap_err();
        assert!(err.to_string().contains("no such session"));
    }

    #[test]
    fn remove_deletes_a_clean_single_repo_session() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let created = create(root.path(), "feature").unwrap();
        assert!(created.root.exists());

        let outcome = remove(root.path(), "feature", false, noop_agent().as_ref()).unwrap();
        assert!(outcome.removed);
        assert!(outcome.dirty.is_empty());
        // The worktree directory and the state record are both gone.
        assert!(!created.root.exists());
        assert!(sessions_of(root.path()).is_empty());
        // The namespaced branch was deleted in the source repo.
        assert!(!git_cmd(root.path())
            .args(["rev-parse", "--verify", "--quiet", "usagi/feature"])
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn remove_cleans_a_multi_repo_session_including_copied_files() {
        let root = tempfile::tempdir().unwrap();
        init_repo(&root.path().join("app-a"));
        init_repo(&root.path().join("be/be1"));
        fs::write(root.path().join("README.md"), "hi").unwrap();
        let created = create(root.path(), "wip").unwrap();
        assert!(created.root.join("README.md").exists());

        let outcome = remove(root.path(), "wip", false, noop_agent().as_ref()).unwrap();
        assert!(outcome.removed);
        // The whole session tree (worktrees + copied files) is gone.
        assert!(!created.root.exists());
        assert!(sessions_of(root.path()).is_empty());
    }

    #[test]
    fn remove_warns_on_uncommitted_changes_and_forces_through() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let created = create(root.path(), "dirty").unwrap();
        // Make the worktree dirty.
        fs::write(created.root.join("scratch.txt"), "wip").unwrap();

        // Without force: blocked, nothing removed, the dirty worktree reported.
        let outcome = remove(root.path(), "dirty", false, noop_agent().as_ref()).unwrap();
        assert!(!outcome.removed);
        assert_eq!(outcome.dirty, vec![created.root.clone()]);
        assert!(created.root.exists());
        assert_eq!(sessions_of(root.path()), vec!["dirty".to_string()]);

        // With force: removed despite the changes.
        let outcome = remove(root.path(), "dirty", true, noop_agent().as_ref()).unwrap();
        assert!(outcome.removed);
        assert!(!created.root.exists());
        assert!(sessions_of(root.path()).is_empty());
    }

    #[test]
    fn discard_session_without_force_aborts_on_a_dirty_worktree_and_keeps_it() {
        // `remove`'s own clean check intercepts the common dirty case, so exercise
        // `discard_session` directly to cover the race / locked-worktree path:
        // without `force`, a worktree git refuses to remove must abort *before*
        // the session directory is deleted, so uncommitted work is never lost.
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let created = create(root.path(), "wip").unwrap();
        fs::write(created.root.join("scratch.txt"), "uncommitted").unwrap();

        let repo_worktrees = reconcile::list_repo_worktrees(root.path()).unwrap();

        let err =
            reconcile::discard_session(&created.root, "wip", &repo_worktrees, false).unwrap_err();
        assert!(err.to_string().contains("git worktree remove failed"));
        assert!(created.root.exists());
        assert!(created.root.join("scratch.txt").exists());

        // Forced teardown discards the dirty worktree as before.
        reconcile::discard_session(&created.root, "wip", &repo_worktrees, true).unwrap();
        assert!(!created.root.exists());
    }

    #[test]
    fn discard_session_logs_a_failure_to_drop_an_orphaned_branch() {
        // When the session branch cannot be dropped during teardown, the failure
        // must not vanish: it is the "session name permanently unusable" state
        // (the branch lingers and blocks recreating the session). Reproduce it
        // with a *locked* worktree, which prune cannot clear, so the branch stays
        // checked out and `git branch -D` refuses — then assert the failure is
        // routed to the daily error log.
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());

        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let created = create(root.path(), "wip").unwrap();

        // Lock the worktree so the post-removal prune cannot clear its
        // registration; the branch then remains checked out there.
        assert!(git_cmd(root.path())
            .args(["worktree", "lock"])
            .arg(&created.root)
            .status()
            .unwrap()
            .success());

        let repo_worktrees = reconcile::list_repo_worktrees(root.path()).unwrap();
        // Forced teardown is best-effort and still reports success... The branch
        // is the namespaced `usagi/wip`, still checked out in the locked worktree
        // git refuses to drop.
        reconcile::discard_session(&created.root, "usagi/wip", &repo_worktrees, true).unwrap();

        // ...but the orphaned-branch failure was recorded to the daily log.
        let logged: String = fs::read_dir(home.path().join("logs"))
            .unwrap()
            .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect();
        assert!(logged.contains("orphaned"), "log was: {logged}");
        assert!(logged.contains("wip"), "log was: {logged}");

        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn remove_clears_every_per_worktree_file_keyed_by_the_session_path() {
        // Point the data dir at a throwaway home so the per-worktree files are
        // isolated.
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());

        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let created = create(root.path(), "feature").unwrap();
        // Seed every per-worktree store keyed by the session's worktree, as the
        // running agent's hooks, the PR-link scanner, the MCP prompt queue, and the
        // pane snapshotter would, then confirm each file landed.
        agent_state_store::write(
            &created.root,
            crate::domain::agent_phase::AgentPhase::Waiting,
        )
        .unwrap();
        pr_link_store::add(
            &created.root,
            &[crate::domain::workspace_state::PrLink {
                number: 7,
                url: "https://github.com/o/r/pull/7".to_string(),
            }],
        )
        .unwrap();
        agent_prompt_store::set(&created.root, "queued prompt").unwrap();
        open_panes_store::save(
            &created.root,
            0,
            &[open_panes_store::StoredPane {
                kind: open_panes_store::StoredPaneKind::Terminal,
                cli: None,
                label: None,
            }],
        )
        .unwrap();
        // Assert through each store's read API rather than counting directory
        // entries, since the locked stores (PR links, prompts) also drop a `.lock`
        // file in their directory that survives the data file.
        assert!(agent_state_store::read(&created.root).is_some());
        assert!(!pr_link_store::get(&created.root).is_empty());
        assert!(open_panes_store::load(&created.root).is_some());

        // Removing the session wipes all of them, so a session recreated at the
        // same path inherits none of the previous run's state.
        let outcome = remove(root.path(), "feature", false, noop_agent().as_ref()).unwrap();
        assert!(outcome.removed);
        assert!(agent_state_store::read(&created.root).is_none());
        assert!(pr_link_store::get(&created.root).is_empty());
        assert!(open_panes_store::load(&created.root).is_none());
        // The queued prompt is gone too — a take after removal finds nothing.
        assert!(agent_prompt_store::take(&created.root).is_none());

        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn remove_drops_a_ghost_session_whose_worktree_was_never_built() {
        use crate::domain::workspace_state::{BranchStatus, SessionRecord, WorktreeState};

        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        // Record a session whose worktree creation was interrupted: the path
        // under `.usagi/sessions/` never materialised on disk and no branch was
        // ever created, so it is not a registered git worktree. This is the
        // "ghost session" left behind by a partial `session create`.
        let store = WorkspaceStore::new(root.path());
        let ghost_root = root.path().join(".usagi/sessions/ghost");
        let mut state = store.load().unwrap().unwrap_or_default();
        state.sessions.push(SessionRecord {
            name: "ghost".to_string(),
            display_name: None,
            note: None,
            root: ghost_root.clone(),
            worktrees: vec![WorktreeState {
                branch: None,
                path: ghost_root.clone(),
                head: String::new(),
                primary: false,
                upstream: None,
                status: BranchStatus::Local,
                diff: None,
                ahead_behind: None,
                pr: Vec::new(),
                updated_at: Utc::now(),
            }],
            created_at: Utc::now(),
            last_active: None,
        });
        store.save(&state).unwrap();
        assert_eq!(sessions_of(root.path()), vec!["ghost".to_string()]);

        // Removal used to abort on the missing worktree (`git -C <gone> worktree
        // list` fails), stranding the record forever. It now succeeds and drops
        // the record.
        let outcome = remove(root.path(), "ghost", false, noop_agent().as_ref()).unwrap();
        assert!(outcome.removed);
        assert!(outcome.dirty.is_empty());
        assert!(sessions_of(root.path()).is_empty());
    }

    /// Forget session `name` in `state.json` while leaving its on-disk directory
    /// in place — the exact "stray" state reconcile is meant to clean up.
    fn drop_record(root: &Path, name: &str) {
        let store = WorkspaceStore::new(root);
        let mut state = store.load().unwrap().unwrap();
        state.sessions.retain(|s| s.name != name);
        store.save(&state).unwrap();
    }

    fn branch_exists(repo: &Path, branch: &str) -> bool {
        git_cmd(repo)
            .args(["rev-parse", "--verify", "--quiet", branch])
            .status()
            .unwrap()
            .success()
    }

    #[test]
    fn reconcile_is_a_noop_without_a_sessions_directory() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        // No `.usagi/sessions/` exists yet, so there is nothing to reconcile.
        assert!(reconcile(root.path()).unwrap().is_empty());
    }

    #[test]
    fn reconcile_force_removes_an_untracked_session_and_keeps_tracked_ones() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let kept = create(root.path(), "keep").unwrap();
        let stray = create(root.path(), "stray").unwrap();
        // Forget "stray" in state.json while its worktree stays on disk.
        drop_record(root.path(), "stray");

        let removed = reconcile(root.path()).unwrap();

        // The stray worktree directory and its branch are gone...
        assert_eq!(removed, vec![stray.root.clone()]);
        assert!(!stray.root.exists());
        assert!(!branch_exists(root.path(), "usagi/stray"));
        // ...while the tracked session and its branch survive untouched.
        assert!(kept.root.exists());
        assert_eq!(head_branch(&kept.root), "usagi/keep");
        assert!(branch_exists(root.path(), "usagi/keep"));
        assert_eq!(sessions_of(root.path()), vec!["keep".to_string()]);
    }

    #[test]
    fn reconcile_force_removes_a_dirty_untracked_session() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let stray = create(root.path(), "stray").unwrap();
        // Uncommitted work must not stop the sync.
        fs::write(stray.root.join("scratch.txt"), "wip").unwrap();
        drop_record(root.path(), "stray");

        let removed = reconcile(root.path()).unwrap();

        assert_eq!(removed, vec![stray.root.clone()]);
        assert!(!stray.root.exists());
        assert!(!branch_exists(root.path(), "usagi/stray"));
    }

    #[test]
    fn reconcile_ignores_loose_files_under_the_sessions_dir() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        create(root.path(), "keep").unwrap();
        // A loose *file* (not a directory) is not a session: leave it be.
        let loose = root.path().join(".usagi/sessions/NOTES.txt");
        fs::write(&loose, "scratch").unwrap();

        let removed = reconcile(root.path()).unwrap();

        assert!(removed.is_empty());
        assert!(loose.is_file());
    }

    #[test]
    fn reconcile_removes_a_stray_when_no_state_exists() {
        let root = tempfile::tempdir().unwrap();
        // A non-git root with a leftover session directory but no state.json.
        let ghost = root.path().join(".usagi/sessions/ghost");
        fs::create_dir_all(&ghost).unwrap();
        fs::write(ghost.join("leftover.txt"), "x").unwrap();

        let removed = reconcile(root.path()).unwrap();

        assert_eq!(removed, vec![ghost.clone()]);
        assert!(!ghost.exists());
    }

    #[test]
    fn reconcile_removes_a_stray_across_a_multi_repo_workspace() {
        let root = tempfile::tempdir().unwrap();
        init_repo(&root.path().join("app-a"));
        init_repo(&root.path().join("be/be1"));
        fs::write(root.path().join("README.md"), "hi").unwrap();
        let stray = create(root.path(), "wip").unwrap();
        drop_record(root.path(), "wip");

        let removed = reconcile(root.path()).unwrap();

        assert_eq!(removed, vec![stray.root.clone()]);
        assert!(!stray.root.exists());
        // The session branch is gone from every source repository.
        assert!(!branch_exists(&root.path().join("app-a"), "usagi/wip"));
        assert!(!branch_exists(&root.path().join("be/be1"), "usagi/wip"));
    }

    #[test]
    fn create_clears_a_stale_directory_of_the_same_name() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        create(root.path(), "dup").unwrap();
        // Forget the record but leave the worktree behind, as a crash might.
        drop_record(root.path(), "dup");

        // Re-creating "dup" succeeds: reconcile clears the stale tree first.
        let recreated = create(root.path(), "dup").unwrap();

        assert!(recreated.root.exists());
        assert_eq!(head_branch(&recreated.root), "usagi/dup");
        assert_eq!(sessions_of(root.path()), vec!["dup".to_string()]);
    }

    #[test]
    fn create_recovers_from_a_dangling_worktree_registration() {
        // A worktree was registered at the session path on some *other* branch,
        // then its directory was deleted out-of-band (a crash or a manual `rm`),
        // leaving git with a dangling "prunable" registration there. Recreating a
        // session of that name used to fail forever with "missing but already
        // registered worktree"; create now prunes the stale registration first.
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let session_path = root.path().join(".usagi/sessions/review");
        fs::create_dir_all(session_path.parent().unwrap()).unwrap();
        // Register a worktree at the eventual session path on an unrelated branch,
        // then remove the directory — only the dangling registration remains.
        assert!(git_cmd(root.path())
            .args(["worktree", "add", "-q", "-b", "fix/review-findings"])
            .arg(&session_path)
            .status()
            .unwrap()
            .success());
        fs::remove_dir_all(&session_path).unwrap();

        let created = create(root.path(), "review").unwrap();

        assert!(created.root.exists());
        assert_eq!(head_branch(&created.root), "usagi/review");
        assert_eq!(sessions_of(root.path()), vec!["review".to_string()]);
    }

    #[test]
    fn discard_session_unregisters_a_worktree_on_an_unexpected_branch() {
        // A worktree sitting at the session path but on a branch other than the
        // session name must still be unregistered when the session is torn down —
        // matching on the branch alone left the registration behind, orphaned, the
        // moment the directory was deleted (the bug above's root cause).
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let session_root = root.path().join(".usagi/sessions/odd");
        fs::create_dir_all(session_root.parent().unwrap()).unwrap();
        assert!(git_cmd(root.path())
            .args(["worktree", "add", "-q", "-b", "other"])
            .arg(&session_root)
            .status()
            .unwrap()
            .success());

        let repo_worktrees = reconcile::list_repo_worktrees(root.path()).unwrap();
        reconcile::discard_session(&session_root, "odd", &repo_worktrees, true).unwrap();

        // The directory is gone *and* git keeps no dangling registration for it,
        // so a later session named "odd" can reuse the path.
        assert!(!session_root.exists());
        let canon = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let target = canon(&session_root);
        let orphaned = git::list_worktrees(root.path())
            .unwrap()
            .iter()
            .any(|wt| canon(&wt.path) == target);
        assert!(!orphaned, "worktree registration was orphaned");
    }

    #[test]
    fn remove_deletes_the_branch_when_the_worktree_dir_vanished_out_of_band() {
        // A recorded session whose worktree directory was deleted out-of-band (a
        // crash, a manual `rm`, an external cleanup) leaves git with a dangling
        // worktree registration that still holds the session branch checked out.
        // Removing the session must still drop that branch — otherwise the branch
        // (and its registration) outlive the session, and a later `create` of the
        // same name fails forever on "branch already exists" with no record left
        // to `remove`. This is the "name permanently unusable" failure.
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let created = create(root.path(), "stuck").unwrap();
        // Delete just the directory, leaving the branch + registration behind.
        fs::remove_dir_all(&created.root).unwrap();
        assert!(branch_exists(root.path(), "usagi/stuck"));

        let outcome = remove(root.path(), "stuck", false, noop_agent().as_ref()).unwrap();
        assert!(outcome.removed);
        // The orphaned branch is gone, so the name is reusable...
        assert!(!branch_exists(root.path(), "usagi/stuck"));
        assert!(sessions_of(root.path()).is_empty());
        // ...and re-creating the session of the same name succeeds.
        let recreated = create(root.path(), "stuck").unwrap();
        assert_eq!(head_branch(&recreated.root), "usagi/stuck");
    }

    #[test]
    fn remove_also_prunes_other_strays() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let a = create(root.path(), "a").unwrap();
        let b = create(root.path(), "b").unwrap();
        // "b" becomes a stray; removing "a" should sync it away as well.
        drop_record(root.path(), "b");

        let outcome = remove(root.path(), "a", false, noop_agent().as_ref()).unwrap();

        assert!(outcome.removed);
        assert!(!a.root.exists());
        assert!(!b.root.exists());
        assert!(sessions_of(root.path()).is_empty());
    }

    #[test]
    fn workspace_root_strips_a_session_subtree() {
        // A cwd inside a session resolves back to the workspace root.
        assert_eq!(
            workspace_root(Path::new("/repo/.usagi/sessions/mcp")),
            PathBuf::from("/repo")
        );
        // ...including a subdirectory deeper within the session.
        assert_eq!(
            workspace_root(Path::new("/repo/.usagi/sessions/mcp/crate/src")),
            PathBuf::from("/repo")
        );
        // A doubly nested copy stops at the first session segment.
        assert_eq!(
            workspace_root(Path::new("/repo/.usagi/sessions/mcp/.usagi/issues")),
            PathBuf::from("/repo")
        );
    }

    #[test]
    fn workspace_root_leaves_a_plain_path_unchanged() {
        // Not inside a session tree: returned as-is.
        assert_eq!(workspace_root(Path::new("/repo")), PathBuf::from("/repo"));
        // A bare `.usagi` without a `sessions` child is not a session tree.
        assert_eq!(
            workspace_root(Path::new("/repo/.usagi/issues")),
            PathBuf::from("/repo/.usagi/issues")
        );
    }
}
