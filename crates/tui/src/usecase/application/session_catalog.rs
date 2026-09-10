//! Pure projection of Git ref observations into create-session branch choices.

use super::controller::{BranchChoice, SessionBranchCatalog};

/// Build the create-session branch catalog from already observed Git output.
///
/// `refs` is the output of `git for-each-ref --format=%(refname) %(symref)`.
/// `symbolic_head` is the successful output of `git symbolic-ref HEAD`, when
/// available. Command execution remains an infrastructure responsibility.
#[must_use]
pub fn project_branch_catalog(
    refs: &str,
    symbolic_head: Option<&str>,
    configured_default: Option<&str>,
) -> SessionBranchCatalog {
    let branches = parse_branch_choices(refs);
    let default = configured_default
        .filter(|configured| branches.iter().any(|branch| branch.refname == *configured))
        .map(str::to_owned)
        .or_else(|| project_branch_default(&branches, symbolic_head));
    SessionBranchCatalog { branches, default }
}

/// Validate an observed symbolic `HEAD` against the available branch choices.
#[must_use]
pub fn project_branch_default(
    branches: &[BranchChoice],
    symbolic_head: Option<&str>,
) -> Option<String> {
    symbolic_head
        .map(str::trim)
        .filter(|refname| branches.iter().any(|branch| branch.refname == *refname))
        .map(str::to_owned)
}

fn parse_branch_choices(output: &str) -> Vec<BranchChoice> {
    output
        .lines()
        .filter_map(|line| {
            let (refname, symref) = line.split_once(' ').unwrap_or((line, ""));
            let label = if symref.is_empty() {
                refname
                    .strip_prefix("refs/heads/")
                    .map(|name| format!("local:{name}"))
                    .or_else(|| {
                        refname
                            .strip_prefix("refs/remotes/")
                            .map(|name| format!("remote:{name}"))
                    })
            } else {
                remote_default_branch_label(refname, symref)
            }?;
            Some(BranchChoice {
                label,
                refname: refname.to_owned(),
            })
        })
        .collect()
}

fn remote_default_branch_label(refname: &str, symref: &str) -> Option<String> {
    let name = refname.strip_prefix("refs/remotes/")?;
    let remote = name.strip_suffix("/HEAD")?;
    let target_prefix = format!("refs/remotes/{remote}/");
    (!remote.is_empty() && symref.starts_with(&target_prefix) && symref != refname)
        .then(|| format!("remote:{remote}/(default)"))
}

#[cfg(test)]
mod tests {
    use super::project_branch_catalog;
    use crate::usecase::application::controller::BranchChoice;

    #[test]
    fn projection_includes_remote_defaults_and_skips_symbolic_aliases() {
        let catalog = project_branch_catalog(
            "refs/heads/main \nrefs/heads/feature \nrefs/heads/current refs/heads/main\nrefs/remotes/origin/HEAD refs/remotes/origin/main\nrefs/remotes/origin/alias refs/remotes/origin/main\nrefs/remotes/origin/main \nrefs/remotes/upstream/HEAD refs/remotes/other/main\nnot-a-ref\n",
            Some("refs/heads/main\n"),
            None,
        );
        assert_eq!(
            catalog.branches,
            vec![
                BranchChoice {
                    label: "local:main".into(),
                    refname: "refs/heads/main".into(),
                },
                BranchChoice {
                    label: "local:feature".into(),
                    refname: "refs/heads/feature".into(),
                },
                BranchChoice {
                    label: "remote:origin/(default)".into(),
                    refname: "refs/remotes/origin/HEAD".into(),
                },
                BranchChoice {
                    label: "remote:origin/main".into(),
                    refname: "refs/remotes/origin/main".into(),
                },
            ]
        );
        assert_eq!(catalog.default.as_deref(), Some("refs/heads/main"));
    }

    #[test]
    fn configured_default_wins_and_stale_observations_are_ignored() {
        let refs = "refs/heads/main \nrefs/heads/feature \n";
        assert_eq!(
            project_branch_catalog(refs, Some("refs/heads/main\n"), Some("refs/heads/feature"))
                .default
                .as_deref(),
            Some("refs/heads/feature")
        );
        assert_eq!(
            project_branch_catalog(
                refs,
                Some("refs/heads/missing\n"),
                Some("refs/heads/missing")
            )
            .default,
            None
        );
    }
}
