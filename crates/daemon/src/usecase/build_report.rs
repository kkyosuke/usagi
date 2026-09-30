//! Which build the running daemon was started from, as a lifecycle report
//! names it.
//!
//! Every lifecycle line is prefixed with [`AppInfo::describe`](usagi_core::domain::AppInfo::describe),
//! which is the version of the binary the operator just ran — not of the daemon
//! it found. After an update, before any rollover, the two differ, and a line
//! that shows only the first reads as "the daemon is already updated". The
//! daemon's own build is the one its handshake advertises; this module is where
//! that observation becomes words, and where a mismatch with this client is
//! said out loud.

use usagi_core::infrastructure::ipc::BuildIdentity;

/// The daemon build a lifecycle verb observed, beside the build of this client.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildObservation {
    /// What the daemon's handshake advertised.
    pub daemon: BuildIdentity,
    /// The build of the process writing the report.
    pub client: BuildIdentity,
}

impl BuildObservation {
    /// The clause a report appends after the daemon's pid.
    #[must_use]
    pub fn clause(&self) -> String {
        if self.daemon.differs_from(&self.client) {
            format!(
                "; daemon build {} differs from this client {}",
                self.daemon.label(),
                self.client.label()
            )
        } else {
            format!("; daemon build {}", self.daemon.label())
        }
    }
}

/// The clause for an optional observation. A daemon whose build was not
/// observed keeps the line it always had, rather than claiming a build.
pub(crate) fn clause(build: Option<&BuildObservation>) -> String {
    build.map_or_else(String::new, BuildObservation::clause)
}

#[cfg(test)]
mod tests {
    use super::{BuildObservation, clause};
    use usagi_core::infrastructure::ipc::BuildIdentity;

    fn build(version: &str, commit: &str) -> BuildIdentity {
        BuildIdentity {
            version: version.to_owned(),
            commit: commit.to_owned(),
            target: "test".to_owned(),
            artifact: String::new(),
        }
    }

    #[test]
    fn a_matching_daemon_names_its_build() {
        let observation = BuildObservation {
            daemon: build("4.8.8", "abcdef0123"),
            client: build("4.8.8", "abcdef0123"),
        };
        assert_eq!(observation.clause(), "; daemon build v4.8.8 (abcdef0)");
    }

    #[test]
    fn an_older_daemon_is_named_as_differing_from_this_client() {
        let observation = BuildObservation {
            daemon: build("4.8.5", "1111111111"),
            client: build("4.8.8", "2222222222"),
        };
        assert_eq!(
            clause(Some(&observation)),
            "; daemon build v4.8.5 (1111111) differs from this client v4.8.8 (2222222)"
        );
    }

    #[test]
    fn an_unobserved_build_adds_nothing() {
        assert_eq!(clause(None), "");
    }
}
