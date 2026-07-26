//! Deterministic fakes shared by the authority tests.
//!
//! Durability, the current locator, and the standby handshake are the three
//! real-IO seams of this subsystem. Faking exactly those three keeps the state
//! machine, the handoff protocol, and the admission barrier fully exercised
//! without a filesystem, a socket, or a second process.

use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

use usagi_core::domain::id::DaemonGeneration;
use usagi_core::infrastructure::ipc::{
    BuildIdentity, ConnectionId, DaemonGeneration as WireGeneration, GenerationRole as WireRole,
    OWNER_GENERATION_ROUTING_CAPABILITY, OperationId, ProtocolLimits, ProtocolVersion, ServerHello,
    build_identity,
};

use super::handoff::{LocatorObservation, PublishedLocator};
use super::registry::{GenerationRegistry, RegistryFile};
use super::rollover::CurrentLocator;
use super::standby::{BUILD_ARTIFACT_CAPABILITY, GENERATION_HANDOFF_CAPABILITY, StandbyProbe};
use crate::usecase::generation::ProcessIdentity;

/// A canonical artifact identity that differs per `tag`, so "same version,
/// different build" is expressible.
pub fn build(tag: &str) -> BuildIdentity {
    let source = match tag {
        "next" => "a",
        "other" => "b",
        _ => "c",
    }
    .repeat(64);
    build_identity("2.6.0", "fixture", "test-target", "debug", &source)
}

/// An identity no peer can compare.
pub fn unknown_build() -> BuildIdentity {
    let mut identity = build("next");
    identity.artifact.clear();
    identity
}

pub fn process(pid: u32) -> ProcessIdentity {
    ProcessIdentity {
        pid,
        start_identity: format!("start-{pid}"),
        process_group: pid,
    }
}

pub fn operation(name: &str) -> OperationId {
    OperationId(format!("build-rollover-v1-{name}"))
}

/// A `ServerHello` advertising both handoff capabilities.
pub fn hello(generation: DaemonGeneration, artifact: &BuildIdentity) -> ServerHello {
    ServerHello {
        connection_nonce: "nonce".into(),
        connection_id: ConnectionId("connection".into()),
        daemon_generation: WireGeneration(generation.as_str()),
        generation_role: WireRole::Active,
        protocol: ProtocolVersion {
            generation: 1,
            revision: 2,
        },
        capabilities: vec![
            BUILD_ARTIFACT_CAPABILITY.to_owned(),
            GENERATION_HANDOFF_CAPABILITY.to_owned(),
            OWNER_GENERATION_ROUTING_CAPABILITY.to_owned(),
        ],
        build: artifact.clone(),
        limits: ProtocolLimits::default(),
        daemon_process: None,
    }
}

#[derive(Default)]
struct FileState {
    contents: Option<String>,
    writes: usize,
    fail_read: bool,
    fail_write: bool,
}

/// An in-memory registry document with a real compare-and-swap.
#[derive(Default)]
pub struct MemoryRegistryFile {
    state: Mutex<FileState>,
}

impl MemoryRegistryFile {
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The stored document, if any.
    pub fn contents(&self) -> Option<String> {
        self.lock().contents.clone()
    }

    /// Replace the stored document out of band — a concurrent writer.
    pub fn set_contents(&self, contents: Option<&str>) {
        self.lock().contents = contents.map(str::to_owned);
    }

    /// How many compare-and-swaps succeeded.
    pub fn writes(&self) -> usize {
        self.lock().writes
    }

    pub fn fail_read(&self, failing: bool) {
        self.lock().fail_read = failing;
    }

    pub fn fail_write(&self, failing: bool) {
        self.lock().fail_write = failing;
    }

    fn lock(&self) -> MutexGuard<'_, FileState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl RegistryFile for Arc<MemoryRegistryFile> {
    fn read(&self) -> io::Result<Option<String>> {
        let state = self.lock();
        if state.fail_read {
            return Err(io::Error::other("injected registry read failure"));
        }
        Ok(state.contents.clone())
    }

    fn compare_and_write(&self, expected: Option<&str>, contents: &str) -> io::Result<bool> {
        let mut state = self.lock();
        if state.fail_write {
            return Err(io::Error::other("injected registry write failure"));
        }
        if state.contents.as_deref() != expected {
            return Ok(false);
        }
        state.contents = Some(contents.to_owned());
        state.writes += 1;
        Ok(true)
    }
}

/// A registry over a fresh in-memory document, with the file kept for
/// inspection.
pub fn registry(limit: usize) -> (GenerationRegistry, Arc<MemoryRegistryFile>) {
    let file = MemoryRegistryFile::shared();
    (GenerationRegistry::new(Arc::clone(&file), limit), file)
}

/// Which locator operations are configured to fail.
#[derive(Default)]
struct LocatorFaults {
    read: bool,
    publish: bool,
    retire: bool,
}

#[derive(Default)]
struct LocatorState {
    published: Option<PublishedLocator>,
    unreadable: bool,
    publishes: Vec<PublishedLocator>,
    retires: usize,
    faults: LocatorFaults,
}

/// An in-memory current locator that records every write.
#[derive(Default)]
pub struct MemoryLocator {
    state: Mutex<LocatorState>,
}

impl MemoryLocator {
    /// A locator already naming `published`.
    pub fn naming(published: PublishedLocator) -> Self {
        let locator = Self::default();
        locator.lock().published = Some(published);
        locator
    }

    /// Make the locator unreadable, as a malformed or unsafe file would be.
    pub fn make_unreadable(&self) {
        self.lock().unreadable = true;
    }

    /// Everything published so far, in order.
    pub fn publishes(&self) -> Vec<PublishedLocator> {
        self.lock().publishes.clone()
    }

    /// How many times the locator was retired.
    pub fn retires(&self) -> usize {
        self.lock().retires
    }

    pub fn fail_read(&self, failing: bool) {
        self.lock().faults.read = failing;
    }

    pub fn fail_publish(&self, failing: bool) {
        self.lock().faults.publish = failing;
    }

    pub fn fail_retire(&self, failing: bool) {
        self.lock().faults.retire = failing;
    }

    fn lock(&self) -> MutexGuard<'_, LocatorState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl CurrentLocator for MemoryLocator {
    fn read(&self) -> io::Result<LocatorObservation> {
        let state = self.lock();
        if state.faults.read {
            return Err(io::Error::other("injected locator read failure"));
        }
        if state.unreadable {
            return Ok(LocatorObservation::Unreadable);
        }
        Ok(state
            .published
            .clone()
            .map_or(LocatorObservation::Absent, LocatorObservation::Published))
    }

    fn publish(&self, locator: &PublishedLocator) -> io::Result<()> {
        let mut state = self.lock();
        if state.faults.publish {
            return Err(io::Error::other("injected locator publish failure"));
        }
        state.unreadable = false;
        state.published = Some(locator.clone());
        state.publishes.push(locator.clone());
        Ok(())
    }

    fn retire(&self) -> io::Result<()> {
        let mut state = self.lock();
        if state.faults.retire {
            return Err(io::Error::other("injected locator retire failure"));
        }
        state.unreadable = false;
        state.published = None;
        state.retires += 1;
        Ok(())
    }
}

/// What a [`RecordingProbe`] answers with.
pub enum ProbeReply {
    Hello(Box<ServerHello>),
    Failure(&'static str),
}

/// A standby probe that records how often the private endpoint was touched.
pub struct RecordingProbe {
    reply: ProbeReply,
    calls: Mutex<Vec<String>>,
}

impl RecordingProbe {
    pub fn new(reply: ProbeReply) -> Self {
        Self {
            reply,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// The endpoints probed, in order.
    pub fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl StandbyProbe for RecordingProbe {
    fn hello(&self, endpoint: &str) -> io::Result<ServerHello> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(endpoint.to_owned());
        match &self.reply {
            ProbeReply::Hello(hello) => Ok((**hello).clone()),
            ProbeReply::Failure(message) => Err(io::Error::other(*message)),
        }
    }
}
