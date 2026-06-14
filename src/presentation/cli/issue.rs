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
use clap::Subcommand;
use serde::Serialize;

use crate::domain::issue::{Issue, IssuePriority, IssueStatus, IssueSummary};
use crate::usecase::issue::{self, IssueChanges, IssueFilter, ListedIssue, NewIssue};

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
        /// Markdown body
        #[arg(long, default_value = "")]
        body: String,
        /// Print the created issue as JSON
        #[arg(long)]
        json: bool,
    },
    /// List issues
    List {
        #[arg(long)]
        status: Option<IssueStatus>,
        #[arg(long)]
        priority: Option<IssuePriority>,
        #[arg(long = "label", value_name = "LABEL")]
        label: Option<String>,
        /// Show only issues ready to start (all dependencies done)
        #[arg(long)]
        ready: bool,
        #[arg(long)]
        json: bool,
    },
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
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Search issue titles and bodies (case-insensitive)
    Search {
        query: String,
        #[arg(long)]
        status: Option<IssueStatus>,
        #[arg(long)]
        priority: Option<IssuePriority>,
        #[arg(long = "label", value_name = "LABEL")]
        label: Option<String>,
        #[arg(long)]
        ready: bool,
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
                    body,
                },
            )?;
            Ok(if json {
                json_lines(&issue_json(&created))?
            } else {
                vec![format!("created #{}: {}", created.number, created.title)]
            })
        }
        IssueCommand::List {
            status,
            priority,
            label,
            ready,
            json,
        } => {
            let filter = IssueFilter {
                status,
                priority,
                label,
                ready_only: ready,
            };
            render_listing(issue::list(repo, &filter)?, json)
        }
        IssueCommand::Search {
            query,
            status,
            priority,
            label,
            ready,
            json,
        } => {
            let filter = IssueFilter {
                status,
                priority,
                label,
                ready_only: ready,
            };
            render_listing(issue::search(repo, &query, &filter)?, json)
        }
        IssueCommand::Show { number, json } => match issue::get(repo, number)? {
            Some(issue) if json => json_lines(&issue_json(&issue)),
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
            body,
            json,
        } => {
            let changes = IssueChanges {
                title,
                status,
                priority,
                labels,
                dependson,
                body,
            };
            match issue::update(repo, number, changes)? {
                Some(updated) if json => json_lines(&issue_json(&updated)),
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

/// Render a listing (from `list` or `search`) either as JSON or as aligned
/// human-readable lines.
fn render_listing(items: Vec<ListedIssue>, json: bool) -> Result<Vec<String>> {
    if json {
        let views: Vec<ListItemJson> = items.iter().map(ListItemJson::from).collect();
        return json_lines(&views);
    }
    Ok(render_list(&items))
}

/// Format a listing as aligned, one-line-per-issue text.
fn render_list(items: &[ListedIssue]) -> Vec<String> {
    if items.is_empty() {
        return vec!["No issues found.".to_string()];
    }
    items
        .iter()
        .map(|l| {
            let marker = readiness(l);
            let mut line = format!(
                "#{:<3} {:<12} {:<6} {:<8} {}",
                l.summary.number,
                l.summary.status.as_str(),
                l.summary.priority.as_str(),
                marker,
                l.summary.title,
            );
            if !l.unmet_deps.is_empty() {
                line.push_str(&format!("  (blocked by {})", join_numbers(&l.unmet_deps)));
            }
            line
        })
        .collect()
}

/// The readiness marker shown for a listed issue.
fn readiness(listed: &ListedIssue) -> &'static str {
    if listed.summary.status == IssueStatus::Done {
        "done"
    } else if listed.is_ready() {
        "ready"
    } else {
        "blocked"
    }
}

fn join_numbers(numbers: &[u32]) -> String {
    numbers
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Serialize `value` to pretty JSON and return it split into lines.
fn json_lines<T: Serialize>(value: &T) -> Result<Vec<String>> {
    let text = serde_json::to_string_pretty(value)?;
    Ok(text.lines().map(str::to_string).collect())
}

/// JSON view of a full issue (including the body).
#[derive(Serialize)]
struct IssueJson<'a> {
    number: u32,
    title: &'a str,
    status: IssueStatus,
    priority: IssuePriority,
    labels: &'a [String],
    dependson: &'a [u32],
    created_at: String,
    updated_at: String,
    body: &'a str,
}

fn issue_json(issue: &Issue) -> IssueJson<'_> {
    IssueJson {
        number: issue.number,
        title: &issue.title,
        status: issue.status,
        priority: issue.priority,
        labels: &issue.labels,
        dependson: &issue.dependson,
        created_at: issue.created_at.to_rfc3339(),
        updated_at: issue.updated_at.to_rfc3339(),
        body: &issue.body,
    }
}

/// JSON view of a listed issue: its metadata plus dependency readiness.
#[derive(Serialize)]
struct ListItemJson<'a> {
    #[serde(flatten)]
    summary: &'a IssueSummary,
    ready: bool,
    unmet_deps: &'a [u32],
}

impl<'a> From<&'a ListedIssue> for ListItemJson<'a> {
    fn from(listed: &'a ListedIssue) -> Self {
        ListItemJson {
            summary: &listed.summary,
            ready: listed.is_ready(),
            unmet_deps: &listed.unmet_deps,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create(repo: &Path, title: &str, deps: Vec<u32>) {
        execute(
            repo,
            IssueCommand::Create {
                title: title.to_string(),
                priority: IssuePriority::Medium,
                labels: vec![],
                dependson: deps,
                body: String::new(),
                json: false,
            },
        )
        .unwrap();
    }

    #[test]
    fn create_reports_the_new_number_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        let lines = execute(
            repo,
            IssueCommand::Create {
                title: "First task".to_string(),
                priority: IssuePriority::High,
                labels: vec!["cli".to_string()],
                dependson: vec![],
                body: "details".to_string(),
                json: false,
            },
        )
        .unwrap();

        assert_eq!(lines, vec!["created #1: First task"]);
        assert_eq!(
            issue::get(repo, 1).unwrap().unwrap().priority,
            IssuePriority::High
        );
    }

    #[test]
    fn create_with_json_emits_the_issue() {
        let tmp = tempfile::tempdir().unwrap();
        let lines = execute(
            tmp.path(),
            IssueCommand::Create {
                title: "T".to_string(),
                priority: IssuePriority::Low,
                labels: vec![],
                dependson: vec![],
                body: String::new(),
                json: true,
            },
        )
        .unwrap();
        let json = lines.join("\n");
        assert!(json.contains("\"number\": 1"));
        assert!(json.contains("\"priority\": \"low\""));
    }

    #[test]
    fn list_marks_ready_blocked_and_done() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        create(repo, "base", vec![]);
        create(repo, "blocked", vec![1]);

        let lines = execute(
            repo,
            IssueCommand::List {
                status: None,
                priority: None,
                label: None,
                ready: false,
                json: false,
            },
        )
        .unwrap();

        assert!(lines[0].contains("#1"));
        assert!(lines[0].contains("ready"));
        assert!(lines[1].contains("#2"));
        assert!(lines[1].contains("blocked"));
        assert!(lines[1].contains("(blocked by 1)"));
    }

    #[test]
    fn list_reports_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let lines = execute(
            tmp.path(),
            IssueCommand::List {
                status: None,
                priority: None,
                label: None,
                ready: false,
                json: false,
            },
        )
        .unwrap();
        assert_eq!(lines, vec!["No issues found."]);
    }

    #[test]
    fn list_ready_only_and_json() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        create(repo, "base", vec![]);
        create(repo, "blocked", vec![1]);

        // ready filter keeps only #1.
        let ready = execute(
            repo,
            IssueCommand::List {
                status: None,
                priority: None,
                label: None,
                ready: true,
                json: false,
            },
        )
        .unwrap();
        assert_eq!(ready.len(), 1);
        assert!(ready[0].contains("#1"));

        // JSON output carries the readiness annotation.
        let json = execute(
            repo,
            IssueCommand::List {
                status: None,
                priority: None,
                label: None,
                ready: false,
                json: true,
            },
        )
        .unwrap()
        .join("\n");
        assert!(json.contains("\"ready\": true"));
        assert!(json.contains("\"unmet_deps\""));
    }

    #[test]
    fn done_issue_is_marked_done_in_listing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        create(repo, "task", vec![]);
        execute(
            repo,
            IssueCommand::Update {
                number: 1,
                title: None,
                status: Some(IssueStatus::Done),
                priority: None,
                labels: None,
                dependson: None,
                body: None,
                json: false,
            },
        )
        .unwrap();

        let lines = render_list(&issue::list(repo, &IssueFilter::default()).unwrap());
        assert!(lines[0].contains("done"));
    }

    #[test]
    fn show_renders_markdown_or_json_or_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        create(repo, "Visible", vec![]);

        let md = execute(
            repo,
            IssueCommand::Show {
                number: 1,
                json: false,
            },
        )
        .unwrap();
        assert!(md.iter().any(|l| l.contains("title: Visible")));

        let json = execute(
            repo,
            IssueCommand::Show {
                number: 1,
                json: true,
            },
        )
        .unwrap()
        .join("\n");
        assert!(json.contains("\"body\""));

        let missing = execute(
            repo,
            IssueCommand::Show {
                number: 9,
                json: false,
            },
        )
        .unwrap();
        assert_eq!(missing, vec!["no issue #9"]);
    }

    #[test]
    fn update_changes_fields_or_reports_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        create(repo, "Old", vec![]);

        let lines = execute(
            repo,
            IssueCommand::Update {
                number: 1,
                title: Some("New".to_string()),
                status: None,
                priority: None,
                labels: Some(vec!["x".to_string()]),
                dependson: Some(vec![2]),
                body: Some("b".to_string()),
                json: false,
            },
        )
        .unwrap();
        assert_eq!(lines, vec!["updated #1: New"]);
        let stored = issue::get(repo, 1).unwrap().unwrap();
        assert_eq!(stored.labels, vec!["x"]);
        assert_eq!(stored.dependson, vec![2]);

        // JSON variant.
        let json = execute(
            repo,
            IssueCommand::Update {
                number: 1,
                title: None,
                status: Some(IssueStatus::InProgress),
                priority: None,
                labels: None,
                dependson: None,
                body: None,
                json: true,
            },
        )
        .unwrap()
        .join("\n");
        assert!(json.contains("\"status\": \"in-progress\""));

        // Missing issue.
        let missing = execute(
            repo,
            IssueCommand::Update {
                number: 9,
                title: None,
                status: None,
                priority: None,
                labels: None,
                dependson: None,
                body: None,
                json: false,
            },
        )
        .unwrap();
        assert_eq!(missing, vec!["no issue #9"]);
    }

    #[test]
    fn search_filters_and_supports_json() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        create(repo, "Login bug", vec![]);
        create(repo, "Unrelated", vec![]);

        let hits = execute(
            repo,
            IssueCommand::Search {
                query: "login".to_string(),
                status: None,
                priority: None,
                label: None,
                ready: false,
                json: false,
            },
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].contains("Login bug"));

        let json = execute(
            repo,
            IssueCommand::Search {
                query: "login".to_string(),
                status: None,
                priority: None,
                label: None,
                ready: false,
                json: true,
            },
        )
        .unwrap()
        .join("\n");
        assert!(json.contains("Login bug"));
    }

    #[test]
    fn delete_requires_yes_and_reports_outcome() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        create(repo, "Doomed", vec![]);

        // Without --yes nothing is deleted.
        let refused = execute(
            repo,
            IssueCommand::Delete {
                number: 1,
                yes: false,
            },
        )
        .unwrap();
        assert_eq!(refused, vec!["pass --yes to delete #1"]);
        assert!(issue::get(repo, 1).unwrap().is_some());

        // With --yes it is deleted.
        let deleted = execute(
            repo,
            IssueCommand::Delete {
                number: 1,
                yes: true,
            },
        )
        .unwrap();
        assert_eq!(deleted, vec!["deleted #1"]);

        // Deleting a missing issue reports so.
        let missing = execute(
            repo,
            IssueCommand::Delete {
                number: 1,
                yes: true,
            },
        )
        .unwrap();
        assert_eq!(missing, vec!["no issue #1"]);
    }

    #[test]
    fn execute_propagates_store_errors() {
        let tmp = tempfile::tempdir().unwrap();
        // A file where the `.usagi` directory should be makes the store fail,
        // and the error propagates out of `execute`.
        std::fs::write(tmp.path().join(".usagi"), "blocker").unwrap();
        let result = execute(
            tmp.path(),
            IssueCommand::Create {
                title: "boom".to_string(),
                priority: IssuePriority::Medium,
                labels: vec![],
                dependson: vec![],
                body: String::new(),
                json: false,
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn run_executes_against_the_current_directory() {
        // `run` reads the current directory; point it at a throwaway repo.
        let _guard = crate::test_support::process_env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let original = env::current_dir().unwrap();
        env::set_current_dir(tmp.path()).unwrap();
        let result = run(IssueCommand::List {
            status: None,
            priority: None,
            label: None,
            ready: false,
            json: false,
        });
        env::set_current_dir(original).unwrap();
        assert!(result.is_ok());
    }
}
