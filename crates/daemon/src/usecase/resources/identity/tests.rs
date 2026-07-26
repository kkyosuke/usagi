//! What may and may not be treated as a child's identity.

use std::io;

use super::{
    ChildIdentity, ChildObservation, ChildProcessProbe, IDENTITY_SOURCE_OS, IdentityRefusal,
    observe_child, record_child,
};
use crate::usecase::resources::ResourceError;
use crate::usecase::resources::fixture::{FakeProbe, ProbeAnswer, verified};

/// A platform that answers the start token but not the process group. Each shape
/// is a separate type so the observation rules stay readable at the call site.
struct GroupOnlyFails;
impl ChildProcessProbe for GroupOnlyFails {
    fn start_identity(&self, _pid: u32) -> io::Result<String> {
        Ok("os:1".to_owned())
    }
    fn process_group(&self, _pid: u32) -> io::Result<u32> {
        Err(io::Error::new(io::ErrorKind::InvalidData, "garbage"))
    }
}

struct GroupVanished;
impl ChildProcessProbe for GroupVanished {
    fn start_identity(&self, _pid: u32) -> io::Result<String> {
        Ok("os:31".to_owned())
    }
    fn process_group(&self, _pid: u32) -> io::Result<u32> {
        Err(io::Error::from(io::ErrorKind::NotFound))
    }
}

struct GroupDenied;
impl ChildProcessProbe for GroupDenied {
    fn start_identity(&self, _pid: u32) -> io::Result<String> {
        Ok("os:31".to_owned())
    }
    fn process_group(&self, _pid: u32) -> io::Result<u32> {
        Err(io::Error::other("denied"))
    }
}

#[test]
fn a_recorded_identity_is_the_platforms_answer_for_that_exact_pid() {
    let probe = FakeProbe::new().with(
        7,
        ProbeAnswer::Alive {
            start: "os:991".to_owned(),
            group: 7,
        },
    );
    let identity = record_child(&probe, 7).unwrap();
    assert_eq!(identity.source, IDENTITY_SOURCE_OS);
    assert_eq!(identity.start_identity, "os:991");
    assert_eq!(identity.process_group, 7);
    assert!(identity.is_verifiable());

    let process = identity.to_process_identity().unwrap();
    assert_eq!(process.pid, 7);
    assert_eq!(process.start_identity, "os:991");
    assert_eq!(process.process_group, 7);
}

#[test]
fn an_unobservable_platform_never_produces_an_identity() {
    let gone = FakeProbe::new().with(1, ProbeAnswer::Gone);
    assert_eq!(record_child(&gone, 1), Err(IdentityRefusal::Gone));
    assert_eq!(record_child(&gone, 404), Err(IdentityRefusal::Gone));
    assert_eq!(
        gone.process_group(1).unwrap_err().kind(),
        io::ErrorKind::NotFound,
        "a gone process answers neither question"
    );

    let denied = FakeProbe::new().with(2, ProbeAnswer::Denied);
    assert_eq!(record_child(&denied, 2), Err(IdentityRefusal::Unobservable));

    let malformed = FakeProbe::new().with(3, ProbeAnswer::Malformed);
    assert_eq!(
        record_child(&malformed, 3),
        Err(IdentityRefusal::Malformed),
        "an empty token is not an identity"
    );

    assert_eq!(
        record_child(&GroupOnlyFails, 4),
        Err(IdentityRefusal::Malformed)
    );
}

#[test]
fn a_fixed_token_is_recorded_as_unverifiable_and_can_never_be_authority() {
    let legacy = ChildIdentity::unverifiable(11, "start");
    assert!(!legacy.is_verifiable());
    assert_eq!(
        legacy.to_process_identity().unwrap_err(),
        ResourceError::IdentityUnverifiable
    );
    let probe = FakeProbe::new().with(
        11,
        ProbeAnswer::Alive {
            start: "start".to_owned(),
            group: 11,
        },
    );
    assert_eq!(
        observe_child(&probe, &legacy),
        ChildObservation::Unknown,
        "a live pid whose token was never observed proves nothing"
    );
    assert!(!ChildIdentity::unverifiable(11, String::new()).is_verifiable());
}

#[test]
fn observation_separates_exact_gone_reuse_and_unknown() {
    let recorded = verified(21, "os:555");
    let mut probe = FakeProbe::new().with(
        21,
        ProbeAnswer::Alive {
            start: "os:555".to_owned(),
            group: 21,
        },
    );
    assert_eq!(observe_child(&probe, &recorded), ChildObservation::Exact);
    assert!(!ChildObservation::Exact.is_definitely_gone());

    probe.set(21, ProbeAnswer::Gone);
    assert_eq!(observe_child(&probe, &recorded), ChildObservation::Gone);
    assert!(ChildObservation::Gone.is_definitely_gone());

    probe.set(
        21,
        ProbeAnswer::Alive {
            start: "os:777".to_owned(),
            group: 21,
        },
    );
    assert_eq!(
        observe_child(&probe, &recorded),
        ChildObservation::Reused,
        "the pid is alive but it is somebody else"
    );
    assert!(ChildObservation::Reused.is_definitely_gone());

    probe.set(21, ProbeAnswer::Denied);
    assert_eq!(observe_child(&probe, &recorded), ChildObservation::Unknown);
    assert!(!ChildObservation::Unknown.is_definitely_gone());
}

#[test]
fn a_matching_token_with_a_different_process_group_is_not_an_exact_match() {
    let recorded = verified(31, "os:31");
    let mismatched = FakeProbe::new().with(
        31,
        ProbeAnswer::Alive {
            start: "os:31".to_owned(),
            group: 99,
        },
    );
    assert_eq!(
        observe_child(&mismatched, &recorded),
        ChildObservation::Unknown
    );

    assert_eq!(
        observe_child(&GroupVanished, &recorded),
        ChildObservation::Gone
    );

    assert_eq!(
        observe_child(&GroupDenied, &recorded),
        ChildObservation::Unknown
    );
}
