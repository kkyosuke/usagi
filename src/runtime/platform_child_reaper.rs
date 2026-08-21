//! Bounded asynchronous reaping for platform helper processes.

use std::io;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::Duration;

const MAX_TRACKED_CHILDREN: usize = 32;
const REAP_INTERVAL: Duration = Duration::from_millis(20);

struct ReaperState {
    children: Mutex<Vec<Child>>,
}

/// Cloneable admission handle for one TUI process's platform helpers.
///
/// Capacity is checked before spawning while holding the child-set lock. The
/// caller never waits for that lock: a busy or full reaper is a safe failure
/// and therefore cannot create an untracked child.
#[derive(Clone)]
pub(crate) struct PlatformChildReaper {
    state: Weak<ReaperState>,
    _lifetime: Arc<()>,
}

impl Default for PlatformChildReaper {
    #[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=platform_child_reaper_reaps_short_helpers_around_a_long_lived_child
    fn default() -> Self {
        let state = Arc::new(ReaperState {
            children: Mutex::new(Vec::with_capacity(MAX_TRACKED_CHILDREN)),
        });
        let lifetime = Arc::new(());
        let worker_state = Arc::clone(&state);
        let worker_lifetime = Arc::downgrade(&lifetime);
        if thread::Builder::new()
            .name("usagi-platform-child-reaper".to_owned())
            .spawn(move || reap_children(&worker_state, &worker_lifetime))
            .is_err()
        {
            return Self {
                state: Weak::new(),
                _lifetime: lifetime,
            };
        }
        Self {
            state: Arc::downgrade(&state),
            _lifetime: lifetime,
        }
    }
}

impl PlatformChildReaper {
    /// Spawn and admit a helper without ever waiting on the caller's thread.
    pub(crate) fn spawn(&self, command: &mut Command) -> io::Result<()> {
        let Some(state) = self.state.upgrade() else {
            return Err(io::Error::other("platform child reaper is unavailable"));
        };
        let mut children = state.children.try_lock().map_err(|_| {
            io::Error::new(io::ErrorKind::WouldBlock, "platform child reaper is busy")
        })?;
        if children.len() >= MAX_TRACKED_CHILDREN {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "too many platform helpers",
            ));
        }
        children.push(command.spawn()?);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn tracked(&self) -> Option<usize> {
        self.state
            .upgrade()?
            .children
            .try_lock()
            .ok()
            .map(|children| children.len())
    }
}

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=platform_child_reaper_reaps_short_helpers_around_a_long_lived_child
fn reap_children(state: &ReaperState, lifetime: &Weak<()>) {
    loop {
        let accepting = lifetime.upgrade().is_some();
        let empty = {
            let mut children = state
                .children
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut index = children.len();
            while index > 0 {
                index -= 1;
                if matches!(children[index].try_wait(), Ok(Some(_))) {
                    let mut child = children.swap_remove(index);
                    // `try_wait` already reaped it; `wait` returns the cached
                    // status and makes the ownership contract explicit.
                    let _ = child.wait();
                }
            }
            children.is_empty()
        };
        if !accepting && empty {
            return;
        }
        thread::sleep(REAP_INTERVAL);
    }
}

#[cfg(all(test, unix))]
mod tests {
    #![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=platform_child_reaper_reaps_short_helpers_around_a_long_lived_child

    use std::process::{Command, Stdio};
    use std::sync::{Arc, Weak};
    use std::time::{Duration, Instant};

    use super::{MAX_TRACKED_CHILDREN, PlatformChildReaper};

    fn helper(script: &str) -> Command {
        let mut command = Command::new("sh");
        command
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    fn wait_until(timeout: Duration, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + timeout;
        while !condition() {
            assert!(Instant::now() < deadline, "condition did not become true");
            std::thread::yield_now();
        }
    }

    fn spawn(reaper: &PlatformChildReaper, command: &mut Command) {
        while let Err(error) = reaper.spawn(command) {
            assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
            std::thread::yield_now();
        }
    }

    #[test]
    fn platform_child_reaper_reaps_short_helpers_around_a_long_lived_child() {
        let temporary = tempfile::tempdir().unwrap();
        let blocker = temporary.path().join("block");
        std::fs::write(&blocker, "").unwrap();
        let reaper = PlatformChildReaper::default();
        let mut long_lived = helper(&format!(
            "while test -e '{}'; do sleep 0.01; done",
            blocker.display()
        ));
        spawn(&reaper, &mut long_lived);
        for _ in 0..16 {
            spawn(&reaper, &mut helper("exit 0"));
        }

        wait_until(Duration::from_secs(5), || reaper.tracked() == Some(1));
        std::fs::remove_file(blocker).unwrap();
        wait_until(Duration::from_secs(5), || reaper.tracked() == Some(0));
    }

    #[test]
    fn platform_child_reaper_rejects_capacity_before_spawning() {
        let temporary = tempfile::tempdir().unwrap();
        let blocker = temporary.path().join("block");
        let overflow_marker = temporary.path().join("overflow");
        std::fs::write(&blocker, "").unwrap();
        let reaper = PlatformChildReaper::default();
        for _ in 0..MAX_TRACKED_CHILDREN {
            let mut command = helper(&format!(
                "while test -e '{}'; do sleep 0.01; done",
                blocker.display()
            ));
            spawn(&reaper, &mut command);
        }
        wait_until(Duration::from_secs(5), || {
            reaper.tracked() == Some(MAX_TRACKED_CHILDREN)
        });

        let mut overflow = helper(&format!("touch '{}'", overflow_marker.display()));
        let overflow_error = loop {
            let error = reaper.spawn(&mut overflow).unwrap_err();
            if error.to_string() == "platform child reaper is busy" {
                std::thread::yield_now();
            } else {
                break error;
            }
        };
        assert_eq!(overflow_error.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(overflow_error.to_string(), "too many platform helpers");
        assert!(!overflow_marker.exists());

        std::fs::remove_file(blocker).unwrap();
        wait_until(Duration::from_secs(5), || reaper.tracked() == Some(0));
    }

    #[test]
    fn platform_child_reaper_does_not_admit_spawn_failures_or_lock_contention() {
        let temporary = tempfile::tempdir().unwrap();
        let marker = temporary.path().join("marker");
        let reaper = PlatformChildReaper::default();
        let mut missing = Command::new(temporary.path().join("missing-helper"));
        assert_eq!(
            reaper.spawn(&mut missing).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        // `tracked` reads through a `try_lock`, so it answers `None` while the
        // reaper thread holds the children. A single sample turns that ordinary
        // contention into a failure on a loaded machine (the coverage run is
        // instrumented and slow), so the observation is driven until it lands.
        wait_until(Duration::from_secs(5), || reaper.tracked() == Some(0));

        let unavailable = PlatformChildReaper {
            state: Weak::new(),
            _lifetime: Arc::new(()),
        };
        assert_eq!(
            unavailable.spawn(&mut helper("exit 0")).unwrap_err().kind(),
            std::io::ErrorKind::Other
        );

        let state = reaper.state.upgrade().unwrap();
        let guard = state.children.lock().unwrap();
        let mut blocked = helper(&format!("touch '{}'", marker.display()));
        assert_eq!(
            reaper.spawn(&mut blocked).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert!(!marker.exists());
        drop(guard);
    }

    #[test]
    fn platform_child_reaper_finishes_reaping_after_admission_closes() {
        let reaper = PlatformChildReaper::default();
        let state = reaper.state.clone();
        spawn(&reaper, &mut helper("exit 0"));
        drop(reaper);

        wait_until(Duration::from_secs(5), || state.upgrade().is_none());
    }
}
