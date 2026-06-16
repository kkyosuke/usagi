//! Claude Code adapter.
//!
//! Builds Claude's launch command (delegating to the pure
//! [`AgentCli::launch_command`], which wires in usagi's MCP servers, system
//! prompt, and lifecycle hooks) and reads its context-window usage from the
//! transcript Claude Code appends under `~/.claude/projects/<encoded-cwd>/<id>.jsonl`.
//! Usage results are cached by the transcript's mtime so the home screen's 200ms
//! watcher does not re-read (and re-parse) an unchanged file every tick.
//!
//! The launch rendering and transcript parsing are pure and tested elsewhere
//! (the domain settings and the parent module); only the filesystem I/O here
//! (locating and reading the transcript) is excluded from coverage (see
//! `scripts/coverage.sh`).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use crate::domain::agent::{Agent, AgentWiring};
use crate::domain::agent_usage::AgentUsage;
use crate::domain::settings::AgentCli;

use super::{encode_project_dir, parse_claude_transcript};

/// The Claude Code adapter, with an mtime cache keyed by the transcript path so
/// an unchanged file is parsed at most once.
#[derive(Default)]
pub struct ClaudeAgent {
    cache: Mutex<HashMap<PathBuf, Cached>>,
}

/// A cached parse: the transcript's mtime when it was read, and the usage it
/// yielded (which may be `None` for a transcript with no usage yet).
struct Cached {
    mtime: SystemTime,
    usage: Option<AgentUsage>,
}

impl ClaudeAgent {
    /// A Claude adapter with an empty usage cache.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Agent for ClaudeAgent {
    fn program(&self) -> &'static str {
        "claude"
    }

    fn launch_command(&self, wiring: &AgentWiring) -> String {
        AgentCli::Claude.launch_command(wiring.local_llm_model.as_deref(), &wiring.usagi_bin)
    }

    fn usage(&self, worktree: &Path) -> Option<AgentUsage> {
        let dir = projects_root()?.join(encode_project_dir(worktree));
        let transcript = newest_jsonl(&dir)?;
        let mtime = fs::metadata(&transcript).ok()?.modified().ok()?;

        // Reuse the cached parse while the transcript is untouched.
        if let Ok(cache) = self.cache.lock() {
            if let Some(cached) = cache.get(&transcript) {
                if cached.mtime == mtime {
                    return cached.usage;
                }
            }
        }

        let usage = fs::read_to_string(&transcript)
            .ok()
            .and_then(|contents| parse_claude_transcript(&contents));
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(transcript, Cached { mtime, usage });
        }
        usage
    }
}

/// `~/.claude/projects`, where Claude Code keeps one directory of transcripts
/// per project working directory.
fn projects_root() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".claude").join("projects"))
}

/// The most-recently-modified `*.jsonl` transcript in `dir` (a session can have
/// several across resumes; the newest is the live one), or `None` when the
/// directory is missing or holds no transcript.
fn newest_jsonl(dir: &Path) -> Option<PathBuf> {
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(dir).ok()? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(mtime) = entry.metadata().and_then(|meta| meta.modified()) else {
            continue;
        };
        if newest.as_ref().is_none_or(|(latest, _)| mtime > *latest) {
            newest = Some((mtime, path));
        }
    }
    newest.map(|(_, path)| path)
}
