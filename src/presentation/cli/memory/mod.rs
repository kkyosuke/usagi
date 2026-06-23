//! `usagi memory`: save, list, show, update, search and delete the durable agent
//! memories stored under the current repository's `.usagi/memory/` (see
//! [`crate::usecase::memory`]).
//!
//! A memory is addressed by its `name` (a slug). Saving under an existing name
//! updates it in place. Pass `--json` for machine-readable output (used by
//! scripts and the MCP server).

use std::env;
use std::path::Path;

use anyhow::Result;
use clap::Subcommand;

use crate::domain::memory::MemoryType;
use crate::usecase::memory::{self, MemoryChanges, MemoryFilter, MemoryView, NewMemory};

#[derive(Subcommand)]
pub enum MemoryCommand {
    /// Save a memory (updates it in place if the name already exists)
    Save {
        #[arg(long)]
        name: String,
        #[arg(long)]
        title: String,
        #[arg(long = "type", value_name = "TYPE", default_value_t = MemoryType::Project)]
        kind: MemoryType,
        /// Name of a related memory (repeat for multiple)
        #[arg(long = "related", value_name = "NAME")]
        related: Vec<String>,
        /// Markdown body
        #[arg(long, default_value = "")]
        body: String,
        /// Print the saved memory as JSON
        #[arg(long)]
        json: bool,
    },
    /// List memories (newest first)
    List {
        #[arg(long = "type", value_name = "TYPE")]
        kind: Option<MemoryType>,
        #[arg(long)]
        json: bool,
    },
    /// Show a single memory
    Show {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Update fields of an existing memory (only the given fields change)
    Update {
        name: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long = "type", value_name = "TYPE")]
        kind: Option<MemoryType>,
        /// Replace all related memories (omit to leave unchanged)
        #[arg(long = "related", value_name = "NAME")]
        related: Option<Vec<String>>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Search memory names, titles and bodies (case-insensitive)
    Search {
        query: String,
        #[arg(long = "type", value_name = "TYPE")]
        kind: Option<MemoryType>,
        #[arg(long)]
        json: bool,
    },
    /// Delete a memory (requires --yes)
    Delete {
        name: String,
        /// Confirm deletion
        #[arg(long)]
        yes: bool,
    },
}

/// Entry point for `usagi memory`: run the subcommand against the current
/// repository and print the result.
pub fn run(command: MemoryCommand) -> Result<()> {
    let repo = env::current_dir()?;
    for line in execute(&repo, command)? {
        println!("{line}");
    }
    Ok(())
}

/// Execute a memory subcommand against `repo`, returning the lines to print.
/// Kept separate from [`run`] so the behaviour is testable without touching the
/// process's current directory or stdout.
fn execute(repo: &Path, command: MemoryCommand) -> Result<Vec<String>> {
    match command {
        MemoryCommand::Save {
            name,
            title,
            kind,
            related,
            body,
            json,
        } => {
            let saved = memory::save(
                repo,
                NewMemory {
                    name,
                    title,
                    kind,
                    related,
                    body,
                },
            )?;
            Ok(if json {
                json_lines(&MemoryView::from(&saved))?
            } else {
                vec![format!("saved {} ({})", saved.name, saved.kind)]
            })
        }
        MemoryCommand::List { kind, json } => {
            render_listing(memory::list(repo, &MemoryFilter { kind })?, json)
        }
        MemoryCommand::Show { name, json } => match memory::get(repo, &name)? {
            Some(m) if json => json_lines(&MemoryView::from(&m)),
            Some(m) => Ok(m.to_markdown().lines().map(str::to_string).collect()),
            None => Ok(vec![format!("no memory '{name}'")]),
        },
        MemoryCommand::Update {
            name,
            title,
            kind,
            related,
            body,
            json,
        } => {
            let changes = MemoryChanges {
                title,
                kind,
                related,
                body,
            };
            match memory::update(repo, &name, changes)? {
                Some(updated) if json => json_lines(&MemoryView::from(&updated)),
                Some(updated) => Ok(vec![format!("updated {}", updated.name)]),
                None => Ok(vec![format!("no memory '{name}'")]),
            }
        }
        MemoryCommand::Search { query, kind, json } => {
            render_listing(memory::search(repo, &query, &MemoryFilter { kind })?, json)
        }
        MemoryCommand::Delete { name, yes } => {
            if !yes {
                return Ok(vec![format!("pass --yes to delete '{name}'")]);
            }
            Ok(if memory::delete(repo, &name)? {
                vec![format!("deleted '{name}'")]
            } else {
                vec![format!("no memory '{name}'")]
            })
        }
    }
}

mod render;
use super::render::json_lines;
use render::render_listing;

#[cfg(test)]
mod tests;
