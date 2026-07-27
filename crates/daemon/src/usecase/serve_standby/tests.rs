use std::cell::{Cell, RefCell};
use std::io;

use usagi_core::domain::AppInfo;
use usagi_core::infrastructure::daemon::ShutdownSignal;

use super::{StandbyAuthority, StandbyEndpoint, serve_standby};
use crate::test_support::{FailingShutdown, ImmediateShutdown};

fn info() -> AppInfo {
    AppInfo {
        name: "usagi",
        version: "0.1.0",
    }
}

/// An endpoint whose two verbs are counted and can be made to fail
/// independently.
#[derive(Default)]
struct FakeEndpoint {
    binds: Cell<usize>,
    retires: Cell<usize>,
    fail_bind: bool,
    fail_retire: bool,
}

impl FakeEndpoint {
    fn failing_bind() -> Self {
        Self {
            fail_bind: true,
            ..Self::default()
        }
    }

    fn failing_retire() -> Self {
        Self {
            fail_retire: true,
            ..Self::default()
        }
    }
}

impl StandbyEndpoint for FakeEndpoint {
    fn bind(&self) -> io::Result<()> {
        self.binds.set(self.binds.get() + 1);
        if self.fail_bind {
            Err(io::Error::other("bind failed"))
        } else {
            Ok(())
        }
    }

    fn retire(&self) -> io::Result<()> {
        self.retires.set(self.retires.get() + 1);
        if self.fail_retire {
            Err(io::Error::other("retire failed"))
        } else {
            Ok(())
        }
    }
}

#[derive(Default)]
struct FakeStandbyAuthority {
    preflights: Cell<usize>,
    admits: Cell<usize>,
    releases: Cell<usize>,
    fail_preflight: bool,
    fail_admit: bool,
    fail_release: bool,
}

impl FakeStandbyAuthority {
    fn failing_preflight() -> Self {
        Self {
            fail_preflight: true,
            ..Self::default()
        }
    }

    fn failing_admit() -> Self {
        Self {
            fail_admit: true,
            ..Self::default()
        }
    }

    fn failing_release() -> Self {
        Self {
            fail_release: true,
            ..Self::default()
        }
    }
}

impl StandbyAuthority for FakeStandbyAuthority {
    fn preflight(&self) -> io::Result<()> {
        self.preflights.set(self.preflights.get() + 1);
        if self.fail_preflight {
            Err(io::Error::other("preflight failed"))
        } else {
            Ok(())
        }
    }

    fn admit(&self) -> io::Result<()> {
        self.admits.set(self.admits.get() + 1);
        if self.fail_admit {
            Err(io::Error::other("admit failed"))
        } else {
            Ok(())
        }
    }

    fn release(&self) -> io::Result<()> {
        self.releases.set(self.releases.get() + 1);
        if self.fail_release {
            Err(io::Error::other("release failed"))
        } else {
            Ok(())
        }
    }
}

/// Records the order every seam was driven in.
struct Ordered<'a> {
    events: &'a RefCell<Vec<&'static str>>,
}

impl StandbyEndpoint for Ordered<'_> {
    fn bind(&self) -> io::Result<()> {
        self.events.borrow_mut().push("bind");
        Ok(())
    }

    fn retire(&self) -> io::Result<()> {
        self.events.borrow_mut().push("retire");
        Ok(())
    }
}

impl StandbyAuthority for Ordered<'_> {
    fn preflight(&self) -> io::Result<()> {
        self.events.borrow_mut().push("preflight");
        Ok(())
    }

    fn admit(&self) -> io::Result<()> {
        self.events.borrow_mut().push("admit");
        Ok(())
    }

    fn release(&self) -> io::Result<()> {
        self.events.borrow_mut().push("release");
        Ok(())
    }
}

impl ShutdownSignal for Ordered<'_> {
    fn prepare(&self) -> io::Result<()> {
        self.events.borrow_mut().push("prepare");
        Ok(())
    }

    fn wait(&self) -> io::Result<()> {
        self.events.borrow_mut().push("wait");
        Ok(())
    }
}

struct FailingPrepare;
impl ShutdownSignal for FailingPrepare {
    fn prepare(&self) -> io::Result<()> {
        Err(io::Error::other("prepare failed"))
    }

    fn wait(&self) -> io::Result<()> {
        Ok(())
    }
}

struct BrokenWriter;
impl io::Write for BrokenWriter {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("output"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn binds_before_admitting_and_releases_before_retiring() {
    let events = RefCell::new(Vec::new());
    let seams = Ordered { events: &events };
    serve_standby(&mut Vec::new(), &seams, &seams, &seams, 4242, &info()).unwrap();
    // The preflight refusals are the ordinary ones, so they land before a socket
    // exists at all. After that the endpoint answers before the registry names
    // it, and the registry stops naming it before it stops answering. Neither
    // order is reversible: the first would publish an entry pointing at a socket
    // nobody accepts on, the second would leave a retained standby whose socket
    // is already gone.
    assert_eq!(
        events.into_inner(),
        [
            "prepare",
            "preflight",
            "bind",
            "admit",
            "wait",
            "release",
            "retire"
        ]
    );
}

#[test]
fn reports_standing_by_and_standing_down() {
    let endpoint = FakeEndpoint::default();
    let authority = FakeStandbyAuthority::default();
    let mut buf = Vec::new();
    serve_standby(
        &mut buf,
        &endpoint,
        &authority,
        &ImmediateShutdown,
        4242,
        &info(),
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(buf).unwrap(),
        "usagi v0.1.0: daemon standing by (pid 4242)\nusagi v0.1.0: daemon standby stopped (pid 4242)\n"
    );
    assert_eq!(endpoint.binds.get(), 1);
    assert_eq!(endpoint.retires.get(), 1);
    assert_eq!(authority.admits.get(), 1);
    assert_eq!(authority.releases.get(), 1);
}

#[test]
fn a_failed_preparation_binds_nothing_at_all() {
    let endpoint = FakeEndpoint::default();
    let authority = FakeStandbyAuthority::default();
    let error = serve_standby(
        &mut Vec::new(),
        &endpoint,
        &authority,
        &FailingPrepare,
        4242,
        &info(),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "prepare failed");
    assert_eq!(endpoint.binds.get(), 0);
    assert_eq!(authority.preflights.get(), 0);
    assert_eq!(authority.admits.get(), 0);
}

/// The refusals a standby actually meets in production — no daemon running, an
/// older build owning the directory without registering — must not have created
/// anything inside a data directory this process does not own.
#[test]
fn a_refused_preflight_binds_nothing_at_all() {
    let endpoint = FakeEndpoint::default();
    let authority = FakeStandbyAuthority::failing_preflight();
    let error = serve_standby(
        &mut Vec::new(),
        &endpoint,
        &authority,
        &ImmediateShutdown,
        4242,
        &info(),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "preflight failed");
    assert_eq!(endpoint.binds.get(), 0);
    assert_eq!(endpoint.retires.get(), 0);
    assert_eq!(authority.admits.get(), 0);
    assert_eq!(authority.releases.get(), 0);
}

#[test]
fn a_failed_bind_never_registers_a_standby() {
    let endpoint = FakeEndpoint::failing_bind();
    let authority = FakeStandbyAuthority::default();
    let error = serve_standby(
        &mut Vec::new(),
        &endpoint,
        &authority,
        &ImmediateShutdown,
        4242,
        &info(),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "bind failed");
    // Nothing durable was written, so there is nothing to stand down from: the
    // bind's own rollback owns whatever it created.
    assert_eq!(authority.admits.get(), 0);
    assert_eq!(authority.releases.get(), 0);
    assert_eq!(endpoint.retires.get(), 0);
}

#[test]
fn a_refused_admission_stands_the_endpoint_down() {
    let endpoint = FakeEndpoint::default();
    let authority = FakeStandbyAuthority::failing_admit();
    let error = serve_standby(
        &mut Vec::new(),
        &endpoint,
        &authority,
        &ImmediateShutdown,
        4242,
        &info(),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "admit failed");
    // A refused admission is the ordinary case for a standby (no live active,
    // an unregistered owner, a build that does not match), so it must leave no
    // socket behind.
    assert_eq!(authority.releases.get(), 1);
    assert_eq!(endpoint.retires.get(), 1);
}

#[test]
fn a_wait_failure_and_an_output_failure_both_stand_down() {
    let endpoint = FakeEndpoint::default();
    let authority = FakeStandbyAuthority::default();
    assert!(
        serve_standby(
            &mut Vec::new(),
            &endpoint,
            &authority,
            &FailingShutdown,
            4242,
            &info(),
        )
        .is_err()
    );
    assert_eq!(endpoint.retires.get(), 1);
    assert_eq!(authority.releases.get(), 1);

    let endpoint = FakeEndpoint::default();
    let authority = FakeStandbyAuthority::default();
    assert!(
        serve_standby(
            &mut BrokenWriter,
            &endpoint,
            &authority,
            &ImmediateShutdown,
            4242,
            &info(),
        )
        .is_err()
    );
    assert_eq!(endpoint.retires.get(), 1);
    assert_eq!(authority.releases.get(), 1);
}

/// A release that fails is reported, and the endpoint is still retired.
///
/// The ordering this path protects is the window while the process runs, and
/// that window closes the moment it returns: the socket stops answering whether
/// or not its file survives, so leaving the file behind would only add residue.
/// The retained entry is not made reachable by it either — a handoff refuses a
/// successor whose process it cannot prove alive.
#[test]
fn an_unreleasable_entry_is_reported_and_still_retires_the_endpoint() {
    let endpoint = FakeEndpoint::default();
    let authority = FakeStandbyAuthority::failing_release();
    let error = serve_standby(
        &mut Vec::new(),
        &endpoint,
        &authority,
        &ImmediateShutdown,
        4242,
        &info(),
    )
    .unwrap_err();
    // The release failure is the primary error, not the retirement's success.
    assert_eq!(error.to_string(), "release failed");
    assert_eq!(authority.releases.get(), 1);
    assert_eq!(endpoint.retires.get(), 1);
}

#[test]
fn a_failed_retirement_is_reported_after_the_entry_is_released() {
    let endpoint = FakeEndpoint::failing_retire();
    let authority = FakeStandbyAuthority::default();
    let error = serve_standby(
        &mut Vec::new(),
        &endpoint,
        &authority,
        &ImmediateShutdown,
        4242,
        &info(),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "retire failed");
    assert_eq!(authority.releases.get(), 1);
}

/// The stand-down path is best effort, but it is still ordered: the entry goes
/// before the socket there too. An unwind whose release also fails still gives
/// the socket back, and still reports the failure that started the unwind
/// rather than either cleanup's.
#[test]
fn a_stand_down_whose_release_fails_still_retires_the_endpoint() {
    let endpoint = FakeEndpoint::default();
    let authority = FakeStandbyAuthority {
        fail_admit: true,
        fail_release: true,
        ..FakeStandbyAuthority::default()
    };
    let error = serve_standby(
        &mut Vec::new(),
        &endpoint,
        &authority,
        &ImmediateShutdown,
        4242,
        &info(),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "admit failed");
    assert_eq!(authority.releases.get(), 1);
    assert_eq!(endpoint.retires.get(), 1);
}
