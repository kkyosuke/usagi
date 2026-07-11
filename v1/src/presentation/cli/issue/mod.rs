//! `usagi issue`: create, list, show, update, search and delete the task issues
//! stored under the current repository's `.usagi/issues/` (see
//! [`crate::usecase::issue`]).
//!
//! Listings mark which issues are *ready* — every issue they depend on is
//! `done` — so it is easy to spot the tasks that can be picked up next. Pass
//! `--json` for machine-readable output (used by scripts and the MCP server).

use std::env;
use std::path::Path;

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::domain::issue::{IssuePriority, IssueStatus};
use crate::usecase::issue::{
    self, dependency_tree, stats_line, GroupBy, IssueChanges, IssueFilter, IssueStats, IssueView,
    NewIssue,
};

#[derive(Subcommand)]
pub enum IssueCommand {
    /// Create a new issue
    Create {
        #[arg(long)]
        title: String,
        #[arg(long, default_value_t = IssuePriority::Medium)]
        priority: IssuePriority,
        /// Add a label (repeat for multiple)
        #[arg(long = "label", value_name = "LABEL")]
        labels: Vec<String>,
        /// Number of an issue this one depends on (repeat for multiple)
        #[arg(long = "depends-on", value_name = "NUMBER")]
        dependson: Vec<u32>,
        /// Number of a related (non-blocking) issue (repeat for multiple)
        #[arg(long = "related", value_name = "NUMBER")]
        related: Vec<u32>,
        /// Number of the parent issue this one belongs to
        #[arg(long, value_name = "NUMBER")]
        parent: Option<u32>,
        /// Milestone to group this issue under
        #[arg(long, value_name = "NAME")]
        milestone: Option<String>,
        /// Markdown body
        #[arg(long, default_value = "")]
        body: String,
        /// Print the created issue as JSON
        #[arg(long)]
        json: bool,
    },
    /// List issues
    List {
        #[command(flatten)]
        filter: FilterArgs,
        /// Group the listing by an axis (status, priority, milestone, parent)
        #[arg(long = "group-by", value_name = "AXIS")]
        group_by: Option<GroupBy>,
        #[arg(long)]
        json: bool,
    },
    /// Print the dependency tree (issues nested under what they depend on)
    Graph,
    /// Show a single issue
    Show {
        number: u32,
        #[arg(long)]
        json: bool,
    },
    /// Update fields of an existing issue (only the given fields change)
    Update {
        number: u32,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        status: Option<IssueStatus>,
        #[arg(long)]
        priority: Option<IssuePriority>,
        /// Replace all labels (repeat for multiple; omit to leave unchanged)
        #[arg(long = "label", value_name = "LABEL")]
        labels: Option<Vec<String>>,
        /// Replace all dependencies (omit to leave unchanged)
        #[arg(long = "depends-on", value_name = "NUMBER")]
        dependson: Option<Vec<u32>>,
        /// Replace all related issues (omit to leave unchanged)
        #[arg(long = "related", value_name = "NUMBER")]
        related: Option<Vec<u32>>,
        /// Set the parent issue
        #[arg(long, value_name = "NUMBER", conflicts_with = "clear_parent")]
        parent: Option<u32>,
        /// Clear the parent issue
        #[arg(long)]
        clear_parent: bool,
        /// Set the milestone
        #[arg(long, value_name = "NAME", conflicts_with = "clear_milestone")]
        milestone: Option<String>,
        /// Clear the milestone
        #[arg(long)]
        clear_milestone: bool,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Search issue titles and bodies (case-insensitive)
    Search {
        query: String,
        #[command(flatten)]
        filter: FilterArgs,
        #[arg(long)]
        json: bool,
    },
    /// Delete an issue (requires --yes)
    Delete {
        number: u32,
        /// Confirm deletion
        #[arg(long)]
        yes: bool,
    },
}

/// CLI flags shared by `issue list` and `issue search`, mapped once to the
/// usecase filter.
#[derive(Args)]
pub struct FilterArgs {
    #[arg(long)]
    status: Option<IssueStatus>,
    #[arg(long)]
    priority: Option<IssuePriority>,
    #[arg(long = "label", value_name = "LABEL")]
    label: Option<String>,
    /// Keep only issues whose parent is this number
    #[arg(long, value_name = "NUMBER")]
    parent: Option<u32>,
    /// Keep only issues in this milestone
    #[arg(long, value_name = "NAME")]
    milestone: Option<String>,
    /// Show only issues ready to start (all dependencies done)
    #[arg(long)]
    ready: bool,
}

impl From<FilterArgs> for IssueFilter {
    fn from(args: FilterArgs) -> Self {
        Self {
            status: args.status,
            priority: args.priority,
            label: args.label,
            parent: args.parent,
            milestone: args.milestone,
            ready_only: args.ready,
        }
    }
}

/// Entry point for `usagi issue`: run the subcommand against the current
/// repository and print the result.
pub fn run(command: IssueCommand) -> Result<()> {
    let repo = env::current_dir()?;
    for line in execute(&repo, command)? {
        println!("{line}");
    }
    Ok(())
}

/// Execute an issue subcommand against `repo`, returning the lines to print.
/// Kept separate from [`run`] so the behavior is testable without touching the
/// process's current directory or stdout.
fn execute(repo: &Path, command: IssueCommand) -> Result<Vec<String>> {
    match command {
        IssueCommand::Create {
            title,
            priority,
            labels,
            dependson,
            related,
            parent,
            milestone,
            body,
            json,
        } => {
            let created = issue::create(
                repo,
                NewIssue {
                    title,
                    priority,
                    labels,
                    dependson,
                    related,
                    parent,
                    milestone,
                    body,
                },
            )?;
            Ok(if json {
                json_lines(&IssueView::from(&created))?
            } else {
                vec![format!("created #{}: {}", created.number, created.title)]
            })
        }
        IssueCommand::List {
            filter,
            group_by,
            json,
        } => {
            let filter = IssueFilter::from(filter);
            let items = issue::list(repo, &filter)?;
            match group_by {
                Some(axis) if !json => Ok(render_grouped(items, axis)),
                _ => render_listing(items, json),
            }
        }
        IssueCommand::Graph => {
            let items = issue::list(repo, &IssueFilter::default())?;
            if items.is_empty() {
                return Ok(vec![NO_ISSUES_FOUND.to_string()]);
            }
            let mut lines = dependency_tree(&items);
            lines.push(String::new());
            lines.push(stats_line(&IssueStats::from_listed(&items)));
            Ok(lines)
        }
        IssueCommand::Search {
            query,
            filter,
            json,
        } => {
            let filter = IssueFilter::from(filter);
            render_listing(issue::search(repo, &query, &filter)?, json)
        }
        IssueCommand::Show { number, json } => match issue::get(repo, number)? {
            Some(issue) if json => json_lines(&IssueView::from(&issue)),
            Some(issue) => Ok(issue.to_markdown().lines().map(str::to_string).collect()),
            None => Ok(vec![format!("no issue #{number}")]),
        },
        IssueCommand::Update {
            number,
            title,
            status,
            priority,
            labels,
            dependson,
            related,
            parent,
            clear_parent,
            milestone,
            clear_milestone,
            body,
            json,
        } => {
            let changes = IssueChanges {
                title,
                status,
                priority,
                labels,
                dependson,
                related,
                parent: optional_change(parent, clear_parent),
                milestone: optional_change(milestone, clear_milestone),
                body,
            };
            match issue::update(repo, number, changes)? {
                Some(updated) if json => json_lines(&IssueView::from(&updated)),
                Some(updated) => Ok(vec![format!(
                    "updated #{}: {}",
                    updated.number, updated.title
                )]),
                None => Ok(vec![format!("no issue #{number}")]),
            }
        }
        IssueCommand::Delete { number, yes } => {
            if !yes {
                return Ok(vec![format!("pass --yes to delete #{number}")]);
            }
            Ok(if issue::delete(repo, number)? {
                vec![format!("deleted #{number}")]
            } else {
                vec![format!("no issue #{number}")]
            })
        }
    }
}

/// Translate a `--field VALUE` / `--clear-field` pair into the tri-state an
/// [`IssueChanges`] optional field expects: `Some(None)` clears, `Some(Some(v))`
/// sets, and `None` leaves the field unchanged. A set value wins over a clear
/// flag, though clap rejects passing both.
fn optional_change<T>(value: Option<T>, clear: bool) -> Option<Option<T>> {
    match (value, clear) {
        (Some(v), _) => Some(Some(v)),
        (None, true) => Some(None),
        (None, false) => None,
    }
}

mod render;
use super::render::json_lines;
use render::{render_grouped, render_listing, NO_ISSUES_FOUND};

#[cfg(test)]
mod tests;
