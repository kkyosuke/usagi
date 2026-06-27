//! The CPU and memory a session's process tree is using, and the pure
//! aggregation that totals it from a sampled process table.
//!
//! A session's work runs as a shell and the agent CLI (and any helpers) beneath
//! it — a tree of processes, not one. To show how much a session (or the whole
//! workspace) is consuming, a sampler reads every process's CPU / memory once
//! ([`ProcSample`]) and [`aggregate_by_root`] sums each session's subtree from
//! that table. Keeping the aggregation a pure function of the sample — rather
//! than reaching into the OS itself — is what lets it be tested without spawning
//! a single real process; the live sampling lives behind a trait in
//! [`infrastructure::resource`](crate::infrastructure::resource).

use std::collections::{HashMap, HashSet};

/// One process as read by a sampler: its pid, its parent (so a tree can be
/// rebuilt), and the CPU / memory it is using. `cpu_percent` is a share of a
/// single core (so a busy multi-threaded process can read above 100, exactly as
/// `top` reports it), and `memory_bytes` is its resident set in bytes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcSample {
    /// The process id.
    pub pid: u32,
    /// The parent process id, if the sampler could read one (a root / orphaned
    /// process has none).
    pub parent: Option<u32>,
    /// CPU use as a percentage of one core (may exceed 100 across cores).
    pub cpu_percent: f32,
    /// Resident memory in bytes.
    pub memory_bytes: u64,
}

/// The CPU and memory a session — or the whole workspace — is using, rounded for
/// display so two readings compare equal unless they actually moved (an `f32`
/// would jitter every sample and force a needless repaint). `cpu_percent` is
/// whole percent summed across the process tree (so it can exceed 100 on
/// multiple cores), and `memory_bytes` is the tree's total resident memory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourceUsage {
    /// Whole-percent CPU across the process tree (may exceed 100 on many cores).
    pub cpu_percent: u32,
    /// Total resident memory of the process tree, in bytes.
    pub memory_bytes: u64,
}

/// One mebibyte, the unit memory is shown in below a gibibyte.
const MIB: u64 = 1024 * 1024;
/// One gibibyte, the threshold memory switches to `N.NGB` at.
const GIB: u64 = 1024 * MIB;

impl ResourceUsage {
    /// Whether nothing is being used — no CPU and no memory. A session with no
    /// live process (or one not yet sampled) reads idle, and the workspace total
    /// reads idle when nothing is running, so the sidebar can omit the number
    /// rather than draw a `0% 0MB` that says nothing.
    pub fn is_idle(&self) -> bool {
        self.cpu_percent == 0 && self.memory_bytes == 0
    }

    /// This usage plus `other`'s — summing two process trees (or folding a tree
    /// into a running total) component-wise.
    pub fn combine(self, other: Self) -> Self {
        Self {
            cpu_percent: self.cpu_percent + other.cpu_percent,
            memory_bytes: self.memory_bytes + other.memory_bytes,
        }
    }

    /// The CPU share as a compact label — `8%` — for the sidebar.
    pub fn format_cpu(&self) -> String {
        format!("{}%", self.cpu_percent)
    }

    /// The memory as a compact, human label: whole mebibytes below a gibibyte
    /// (`512MB`), then one decimal of gibibytes (`1.2GB`), so the figure stays
    /// short enough for a session row whatever its scale.
    pub fn format_memory(&self) -> String {
        if self.memory_bytes >= GIB {
            format!("{:.1}GB", self.memory_bytes as f64 / GIB as f64)
        } else {
            format!("{}MB", self.memory_bytes / MIB)
        }
    }
}

/// Sum each root's process subtree from a sampled process table, returning the
/// per-root usage (in the same order as `roots`) and the grand total across all
/// of them.
///
/// Each entry in `roots` pairs a caller key (a session's worktree path, say)
/// with the root pids whose subtrees belong to it — usually one shell pid, but a
/// session may hold several panes. The subtree is walked through the `parent`
/// links in `samples`: a process counts toward a root when it is that root or a
/// transitive child of it. A `visited` guard means a process is counted once
/// even if pid reuse produced a cycle, and a root pid absent from the sample
/// (its process already gone) simply contributes nothing. CPU is summed as the
/// sampled `f32` and rounded once at the end, so a tree of many small shares
/// does not lose a percent to per-process rounding.
pub fn aggregate_by_root<K: Clone>(
    samples: &[ProcSample],
    roots: &[(K, Vec<u32>)],
) -> (Vec<(K, ResourceUsage)>, ResourceUsage) {
    let by_pid: HashMap<u32, &ProcSample> = samples.iter().map(|s| (s.pid, s)).collect();
    // The child pids of each process, so a subtree can be walked downward from
    // its root rather than scanning every sample per root.
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for s in samples {
        if let Some(parent) = s.parent {
            children.entry(parent).or_default().push(s.pid);
        }
    }

    let mut per_root = Vec::with_capacity(roots.len());
    let mut total = ResourceUsage::default();
    for (key, root_pids) in roots {
        let mut cpu = 0.0_f32;
        let mut memory_bytes = 0_u64;
        let mut visited = HashSet::new();
        let mut stack: Vec<u32> = root_pids.clone();
        while let Some(pid) = stack.pop() {
            // Already counted (a shared / reused pid): skip so a cycle can't sum
            // a process twice or loop forever.
            if !visited.insert(pid) {
                continue;
            }
            if let Some(sample) = by_pid.get(&pid) {
                cpu += sample.cpu_percent;
                memory_bytes += sample.memory_bytes;
            }
            if let Some(kids) = children.get(&pid) {
                stack.extend(kids.iter().copied());
            }
        }
        let usage = ResourceUsage {
            cpu_percent: cpu.round() as u32,
            memory_bytes,
        };
        total = total.combine(usage);
        per_root.push((key.clone(), usage));
    }
    (per_root, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(pid: u32, parent: Option<u32>, cpu: f32, mem: u64) -> ProcSample {
        ProcSample {
            pid,
            parent,
            cpu_percent: cpu,
            memory_bytes: mem,
        }
    }

    #[test]
    fn is_idle_only_when_both_are_zero() {
        assert!(ResourceUsage::default().is_idle());
        assert!(!ResourceUsage {
            cpu_percent: 1,
            memory_bytes: 0,
        }
        .is_idle());
        assert!(!ResourceUsage {
            cpu_percent: 0,
            memory_bytes: 1,
        }
        .is_idle());
    }

    #[test]
    fn add_sums_each_component() {
        let a = ResourceUsage {
            cpu_percent: 3,
            memory_bytes: 100,
        };
        let b = ResourceUsage {
            cpu_percent: 4,
            memory_bytes: 200,
        };
        assert_eq!(
            a.combine(b),
            ResourceUsage {
                cpu_percent: 7,
                memory_bytes: 300,
            }
        );
    }

    #[test]
    fn format_cpu_is_a_whole_percent() {
        assert_eq!(
            ResourceUsage {
                cpu_percent: 8,
                memory_bytes: 0,
            }
            .format_cpu(),
            "8%"
        );
    }

    #[test]
    fn format_memory_shows_whole_mib_below_a_gib() {
        assert_eq!(
            ResourceUsage {
                cpu_percent: 0,
                memory_bytes: 120 * MIB,
            }
            .format_memory(),
            "120MB"
        );
        // Rounds down to whole mebibytes (no decimal below a gibibyte).
        assert_eq!(
            ResourceUsage {
                cpu_percent: 0,
                memory_bytes: 120 * MIB + MIB / 2,
            }
            .format_memory(),
            "120MB"
        );
        assert_eq!(
            ResourceUsage {
                cpu_percent: 0,
                memory_bytes: 0,
            }
            .format_memory(),
            "0MB"
        );
    }

    #[test]
    fn format_memory_switches_to_gib_with_one_decimal() {
        assert_eq!(
            ResourceUsage {
                cpu_percent: 0,
                memory_bytes: GIB,
            }
            .format_memory(),
            "1.0GB"
        );
        assert_eq!(
            ResourceUsage {
                cpu_percent: 0,
                memory_bytes: GIB + GIB / 5,
            }
            .format_memory(),
            "1.2GB"
        );
    }

    #[test]
    fn aggregate_sums_a_root_and_its_descendants() {
        // shell(10) → agent(11) → helper(12); a sibling tree under 20.
        let samples = vec![
            sample(10, Some(1), 1.0, 10 * MIB),
            sample(11, Some(10), 2.0, 20 * MIB),
            sample(12, Some(11), 3.0, 30 * MIB),
            sample(20, Some(1), 5.0, 50 * MIB),
        ];
        let roots = vec![("a", vec![10_u32]), ("b", vec![20_u32])];
        let (per_root, total) = aggregate_by_root(&samples, &roots);
        assert_eq!(
            per_root,
            vec![
                (
                    "a",
                    ResourceUsage {
                        cpu_percent: 6, // 1 + 2 + 3
                        memory_bytes: 60 * MIB,
                    }
                ),
                (
                    "b",
                    ResourceUsage {
                        cpu_percent: 5,
                        memory_bytes: 50 * MIB,
                    }
                ),
            ]
        );
        assert_eq!(
            total,
            ResourceUsage {
                cpu_percent: 11,
                memory_bytes: 110 * MIB,
            }
        );
    }

    #[test]
    fn aggregate_rounds_the_summed_cpu_once() {
        // Three 0.4% shares sum to 1.2% → rounds to 1, not three 0%-rounded leaves.
        let samples = vec![
            sample(10, None, 0.4, MIB),
            sample(11, Some(10), 0.4, MIB),
            sample(12, Some(11), 0.4, MIB),
        ];
        let (per_root, _) = aggregate_by_root(&samples, &[("a", vec![10])]);
        assert_eq!(per_root[0].1.cpu_percent, 1);
    }

    #[test]
    fn aggregate_counts_several_roots_for_one_key() {
        // A session with two panes (two shell pids) sums both subtrees.
        let samples = vec![
            sample(10, None, 1.0, 10 * MIB),
            sample(30, None, 4.0, 40 * MIB),
        ];
        let (per_root, _) = aggregate_by_root(&samples, &[("a", vec![10, 30])]);
        assert_eq!(
            per_root[0].1,
            ResourceUsage {
                cpu_percent: 5,
                memory_bytes: 50 * MIB,
            }
        );
    }

    #[test]
    fn aggregate_ignores_a_missing_root_and_breaks_cycles() {
        // pid 99 is not in the sample (its process is gone) → contributes nothing.
        // pids 10↔11 form a parent cycle (pid reuse) → each counted once.
        let samples = vec![
            sample(10, Some(11), 2.0, 20 * MIB),
            sample(11, Some(10), 3.0, 30 * MIB),
        ];
        let (per_root, total) =
            aggregate_by_root(&samples, &[("gone", vec![99]), ("loop", vec![10])]);
        assert_eq!(per_root[0].1, ResourceUsage::default());
        assert_eq!(
            per_root[1].1,
            ResourceUsage {
                cpu_percent: 5,
                memory_bytes: 50 * MIB,
            }
        );
        assert_eq!(
            total,
            ResourceUsage {
                cpu_percent: 5,
                memory_bytes: 50 * MIB,
            }
        );
    }

    #[test]
    fn aggregate_over_no_roots_is_idle() {
        let (per_root, total) = aggregate_by_root::<&str>(&[], &[]);
        assert!(per_root.is_empty());
        assert!(total.is_idle());
    }
}
