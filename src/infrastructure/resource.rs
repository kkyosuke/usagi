//! Sampling real process CPU / memory through [`sysinfo`], behind a trait so the
//! pool's watcher can total a session's process tree without binding to the OS
//! directly — and be handed a fake in tests.
//!
//! This is the live-system I/O layer: it reads every process's CPU / memory from
//! the kernel each tick. The pure work — folding those samples into a per-session
//! and workspace total — lives in [`domain::resource`](crate::domain::resource)
//! and is tested there with synthetic samples, so this file holds only the
//! `sysinfo` call and is excluded from coverage (see `scripts/coverage.sh`),
//! exactly like [`pty`](super::pty).

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

use crate::domain::resource::ProcSample;

/// Reads a snapshot of every process's CPU / memory and parent. Implemented over
/// the live system by [`SysinfoSampler`]; the trait lets the watcher own a
/// sampler it can swap for a fake when tested.
pub trait ResourceSampler: Send {
    /// Refresh and return one [`ProcSample`] per running process.
    fn sample(&mut self) -> Vec<ProcSample>;
}

/// The live sampler over [`sysinfo`]. It keeps the [`System`] between calls
/// because CPU use is measured as the work done *since the previous refresh*: the
/// first sample reads zero CPU, and each later one reflects the interval since the
/// watcher last sampled.
pub struct SysinfoSampler {
    system: System,
}

impl SysinfoSampler {
    /// A sampler with an empty process table; the first [`sample`](Self::sample)
    /// populates it (and reads zero CPU, having no prior refresh to diff against).
    pub fn new() -> Self {
        Self {
            system: System::new(),
        }
    }
}

impl Default for SysinfoSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceSampler for SysinfoSampler {
    fn sample(&mut self) -> Vec<ProcSample> {
        // Refresh only CPU and memory (not the command line, environment, etc.),
        // and drop processes that have exited, so the table stays current and the
        // refresh stays cheap.
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        let processes = self.system.processes();
        // Size the sample buffer to the live process count up front so the per-tick
        // collect does not grow-and-reallocate the Vec as it fills (the table holds
        // every process on the host).
        let mut out = Vec::with_capacity(processes.len());
        out.extend(processes.iter().map(|(pid, process)| ProcSample {
            pid: pid.as_u32(),
            parent: process.parent().map(|p| p.as_u32()),
            cpu_percent: process.cpu_usage(),
            memory_bytes: process.memory(),
            name: process.name().to_string_lossy().to_string(),
        }));
        out
    }
}
