//! Independent verification for the Goal review-ready pull-request contract.
//!
//! Worker output is only a candidate. The verifier canonicalizes that candidate
//! and asks the provider through a fixed argv boundary before producing the
//! redaction-safe fact accepted by the supervisor reducer.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use usagi_core::domain::{
    agent::StructuredResult,
    pr_inventory::{GitHubRepository, PrChecksState, PrState, canonicalize},
    supervisor::{ArtifactContract, ArtifactExpectation, GOAL_REVIEW_READY_ARTIFACT_CONTRACT},
};

use super::{
    pr_inventory::{GhProcessPort, gh_pr_view_argv, parse_gh_pr_view},
    supervisor_runtime::{ArtifactVerification, ArtifactVerificationStatus, ArtifactVerifier},
};

const VERIFY_TIMEOUT_MS: u64 = 5_000;
const GIT_FACT_TIMEOUT_MS: u64 = 2_000;

/// Goal artifact verifier with an injected provider process boundary.
pub struct GoalArtifactVerifier<R> {
    runner: R,
}

impl<R> GoalArtifactVerifier<R> {
    #[must_use]
    pub const fn new(runner: R) -> Self {
        Self { runner }
    }
}

fn evidence_digest(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut digest = Sha256::new();
    digest.update(b"usagi-goal-artifact-v1\0");
    digest.update(value.as_bytes());
    let bytes = digest.finalize();
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    format!("sha256:{encoded}")
}

fn rejected(reason: &'static str) -> ArtifactVerification {
    ArtifactVerification {
        status: ArtifactVerificationStatus::Rejected,
        result_digest: evidence_digest(reason),
        safe_summary: reason.into(),
    }
}

fn retryable(reason: &'static str) -> ArtifactVerification {
    ArtifactVerification {
        status: ArtifactVerificationStatus::Retryable,
        result_digest: evidence_digest(reason),
        safe_summary: reason.into(),
    }
}

/// Resolves the repository before a Goal worker is spawned.
///
/// # Errors
/// Returns a retryable verification fact when trusted Git identity cannot be
/// obtained or validated.
pub fn resolve_artifact_repository<R: GhProcessPort>(
    runner: &mut R,
    workspace_root: &Path,
) -> Result<GitHubRepository, ArtifactVerification> {
    let Some(root) = workspace_root.to_str() else {
        return Err(retryable("workspace Git identity is unavailable"));
    };
    let argv = ["-C", root, "remote", "get-url", "origin"]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let remote = runner
        .run("git", &argv, GIT_FACT_TIMEOUT_MS)
        .map_err(|_| retryable("workspace Git remote is unavailable"))?;
    GitHubRepository::from_remote(remote.trim())
        .ok_or_else(|| retryable("workspace GitHub repository identity is unavailable"))
}

/// Resolves the exact completed revision while retaining the repository pinned
/// before worker spawn. The root path is an argv value, never shell syntax.
///
/// # Errors
/// Returns a retryable verification fact when the Git revision cannot be
/// obtained or validated.
pub fn resolve_artifact_expectation<R: GhProcessPort>(
    runner: &mut R,
    workspace_root: &Path,
    repository: GitHubRepository,
) -> Result<ArtifactExpectation, ArtifactVerification> {
    resolve_artifact_expectation_for_worktrees(runner, &[workspace_root.to_path_buf()], repository)
}

/// Pins every exact supervised checkout which may have produced the reported
/// pull request. A PR head must match at least one of these revisions.
///
/// # Errors
/// Returns a retryable verification fact when any checkout revision cannot be
/// obtained or the resulting bounded head set is invalid.
pub fn resolve_artifact_expectation_for_worktrees<R: GhProcessPort>(
    runner: &mut R,
    workspace_roots: &[PathBuf],
    repository: GitHubRepository,
) -> Result<ArtifactExpectation, ArtifactVerification> {
    let mut heads = Vec::with_capacity(workspace_roots.len());
    for workspace_root in workspace_roots {
        let Some(root) = workspace_root.to_str() else {
            return Err(retryable("workspace Git identity is unavailable"));
        };
        let argv = ["-C", root, "rev-parse", "HEAD"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let head = runner
            .run("git", &argv, GIT_FACT_TIMEOUT_MS)
            .map_err(|_| retryable("workspace Git revision is unavailable"))?;
        heads.push(head.trim().to_owned());
    }
    ArtifactExpectation::from_heads(repository, heads.iter().map(String::as_str))
        .ok_or_else(|| retryable("workspace Git revision is invalid"))
}

impl<R: GhProcessPort> ArtifactVerifier for GoalArtifactVerifier<R> {
    fn verify(
        &mut self,
        contract: ArtifactContract,
        result: Option<&StructuredResult>,
        expectation: &ArtifactExpectation,
        previous_verification_digest: Option<&str>,
    ) -> ArtifactVerification {
        if contract != GOAL_REVIEW_READY_ARTIFACT_CONTRACT {
            return rejected("unsupported artifact contract");
        }
        let Some(candidate) = result.and_then(|value| value.pr.as_deref()) else {
            return rejected("completion did not report a pull request");
        };
        let Some(identity) = canonicalize(candidate.trim()) else {
            return rejected("completion reported an invalid GitHub pull request URL");
        };
        if !identity.belongs_to(expectation.repository()) {
            return rejected("pull request belongs to another repository");
        }
        let Ok(output) = self
            .runner
            .run("gh", &gh_pr_view_argv(&identity), VERIFY_TIMEOUT_MS)
        else {
            return retryable("pull request verification provider is unavailable");
        };
        let Some(view) = parse_gh_pr_view(&output) else {
            return retryable("pull request verification returned an invalid response");
        };
        if view.state != PrState::Open {
            return rejected("pull request is not open");
        }
        if view.draft {
            return rejected("pull request is still a draft");
        }
        if !expectation.matches_head(&view.head_oid) {
            return rejected("pull request head does not match the completed workspace revision");
        }
        if view.checks == Some(PrChecksState::Failing) {
            return rejected("pull request checks are not passing");
        }
        if view.checks == Some(PrChecksState::Pending) {
            return retryable("pull request checks are still pending");
        }
        // Failing and pending returned above. Keep the accepted-state mapping
        // closed over only the two values which can actually reach evidence.
        let (checks, safe_summary) = if view.checks == Some(PrChecksState::Passing) {
            (
                "passing",
                "open review-ready pull request verified with passing checks",
            )
        } else if previous_verification_digest
            == Some(evidence_digest("pull request checks have not appeared yet").as_str())
        {
            (
                "not_configured",
                "open review-ready pull request verified with no configured checks",
            )
        } else {
            return retryable("pull request checks have not appeared yet");
        };
        ArtifactVerification {
            status: ArtifactVerificationStatus::Verified,
            result_digest: evidence_digest(&format!(
                "url={}\nhead={}\nstate=open\ndraft=false\nchecks={checks}",
                identity.as_url(),
                view.head_oid
            )),
            safe_summary: safe_summary.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Clone)]
    struct Runner(VecDeque<Result<String, ()>>);
    impl GhProcessPort for Runner {
        type Error = ();
        fn run(
            &mut self,
            program: &str,
            argv: &[String],
            timeout_ms: u64,
        ) -> Result<String, Self::Error> {
            assert_eq!(program, "gh");
            assert_eq!(timeout_ms, VERIFY_TIMEOUT_MS);
            assert_eq!(argv[0..2], ["pr", "view"]);
            self.0.pop_front().unwrap()
        }
    }

    struct GitRunner {
        outputs: VecDeque<Result<String, ()>>,
        calls: Vec<(String, Vec<String>, u64)>,
    }

    impl GhProcessPort for GitRunner {
        type Error = ();

        fn run(
            &mut self,
            program: &str,
            argv: &[String],
            timeout_ms: u64,
        ) -> Result<String, Self::Error> {
            self.calls
                .push((program.to_owned(), argv.to_vec(), timeout_ms));
            self.outputs.pop_front().unwrap()
        }
    }

    fn result(pr: Option<&str>) -> StructuredResult {
        StructuredResult {
            pr: pr.map(str::to_owned),
            ..StructuredResult::default()
        }
    }

    fn view(state: &str, draft: bool, checks: &str) -> String {
        format!(
            r#"{{"title":"Goal","state":"{state}","headRefOid":"0123456789012345678901234567890123456789","isDraft":{draft},"reviewDecision":"","statusCheckRollup":{checks}}}"#
        )
    }

    fn expectation() -> ArtifactExpectation {
        ArtifactExpectation::new(
            usagi_core::domain::pr_inventory::GitHubRepository::from_name_with_owner("acme/repo")
                .unwrap(),
            "0123456789012345678901234567890123456789",
        )
        .unwrap()
    }

    #[test]
    fn only_an_open_non_draft_pr_with_passing_checks_satisfies_the_contract() {
        let cases = [
            (
                view("OPEN", false, r#"[{"conclusion":"SUCCESS"}]"#),
                ArtifactVerificationStatus::Verified,
            ),
            (
                view("OPEN", true, r#"[{"conclusion":"SUCCESS"}]"#),
                ArtifactVerificationStatus::Rejected,
            ),
            (
                view("CLOSED", false, r#"[{"conclusion":"SUCCESS"}]"#),
                ArtifactVerificationStatus::Rejected,
            ),
            (
                view("OPEN", false, r#"[{"conclusion":"PENDING"}]"#),
                ArtifactVerificationStatus::Retryable,
            ),
            (
                view("OPEN", false, r#"[{"conclusion":"FAILURE"}]"#),
                ArtifactVerificationStatus::Rejected,
            ),
        ];
        for (provider, expected) in cases {
            let mut verifier = GoalArtifactVerifier::new(Runner([Ok(provider)].into()));
            assert_eq!(
                verifier
                    .verify(
                        GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
                        Some(&result(Some("https://github.com/acme/repo/pull/42"))),
                        &expectation(),
                        None,
                    )
                    .status,
                expected
            );
        }

        let mut first_empty =
            GoalArtifactVerifier::new(Runner([Ok(view("OPEN", false, "[]"))].into()));
        let deferred = first_empty.verify(
            GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
            Some(&result(Some("https://github.com/acme/repo/pull/42"))),
            &expectation(),
            None,
        );
        assert_eq!(deferred.status, ArtifactVerificationStatus::Retryable);
        assert_eq!(
            deferred.safe_summary,
            "pull request checks have not appeared yet"
        );
        let mut second_empty =
            GoalArtifactVerifier::new(Runner([Ok(view("OPEN", false, "[]"))].into()));
        assert_eq!(
            second_empty
                .verify(
                    GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
                    Some(&result(Some("https://github.com/acme/repo/pull/42"))),
                    &expectation(),
                    Some(&deferred.result_digest),
                )
                .status,
            ArtifactVerificationStatus::Verified
        );
    }

    #[test]
    fn missing_invalid_and_provider_failures_are_safe_rejections() {
        let mut unavailable = GoalArtifactVerifier::new(Runner([Err(())].into()));
        assert_eq!(
            unavailable
                .verify(
                    GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
                    Some(&result(Some("https://github.com/acme/repo/pull/42"))),
                    &expectation(),
                    None,
                )
                .status,
            ArtifactVerificationStatus::Retryable
        );
        let mut malformed = GoalArtifactVerifier::new(Runner([Ok("not json".into())].into()));
        let malformed = malformed.verify(
            GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
            Some(&result(Some("https://github.com/acme/repo/pull/42"))),
            &expectation(),
            None,
        );
        assert_eq!(malformed.status, ArtifactVerificationStatus::Retryable);
        assert_eq!(
            malformed.safe_summary,
            "pull request verification returned an invalid response"
        );
        let mut never_called = GoalArtifactVerifier::new(Runner(VecDeque::new()));
        for (contract, result) in [
            (
                ArtifactContract::None,
                result(Some("https://github.com/acme/repo/pull/42")),
            ),
            (GOAL_REVIEW_READY_ARTIFACT_CONTRACT, result(None)),
            (
                GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
                result(Some("not a pr")),
            ),
        ] {
            assert_eq!(
                never_called
                    .verify(contract, Some(&result), &expectation(), None)
                    .status,
                ArtifactVerificationStatus::Rejected
            );
        }

        let wrong_repository = never_called.verify(
            GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
            Some(&result(Some("https://github.com/other/repo/pull/42"))),
            &expectation(),
            None,
        );
        assert_eq!(
            wrong_repository.status,
            ArtifactVerificationStatus::Rejected
        );
        assert_eq!(
            wrong_repository.safe_summary,
            "pull request belongs to another repository"
        );

        let wrong_head = ArtifactExpectation::new(
            usagi_core::domain::pr_inventory::GitHubRepository::from_name_with_owner("acme/repo")
                .unwrap(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .unwrap();
        let mut verifier = GoalArtifactVerifier::new(Runner(
            [Ok(view("OPEN", false, r#"[{"conclusion":"SUCCESS"}]"#))].into(),
        ));
        let wrong_head = verifier.verify(
            GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
            Some(&result(Some("https://github.com/acme/repo/pull/42"))),
            &wrong_head,
            None,
        );
        assert_eq!(wrong_head.status, ArtifactVerificationStatus::Rejected);
        assert_eq!(
            wrong_head.safe_summary,
            "pull request head does not match the completed workspace revision"
        );
    }

    #[test]
    fn workspace_git_facts_use_fixed_argv_and_are_normalized_once() {
        let root = Path::new("/tmp/work tree;not-shell");
        let mut runner = GitRunner {
            outputs: [
                Ok("git@github.com:Acme/Repo.git\n".into()),
                Ok("ABCDEF0123456789ABCDEF0123456789ABCDEF01\n".into()),
            ]
            .into(),
            calls: Vec::new(),
        };
        let repository = resolve_artifact_repository(&mut runner, root).unwrap();
        let expectation = resolve_artifact_expectation(&mut runner, root, repository).unwrap();
        assert_eq!(expectation.repository().as_str(), "acme/repo");
        assert_eq!(
            expectation.head_oid(),
            "abcdef0123456789abcdef0123456789abcdef01"
        );
        assert_eq!(
            runner.calls,
            vec![
                (
                    "git".into(),
                    vec![
                        "-C".into(),
                        "/tmp/work tree;not-shell".into(),
                        "remote".into(),
                        "get-url".into(),
                        "origin".into(),
                    ],
                    GIT_FACT_TIMEOUT_MS,
                ),
                (
                    "git".into(),
                    vec![
                        "-C".into(),
                        "/tmp/work tree;not-shell".into(),
                        "rev-parse".into(),
                        "HEAD".into(),
                    ],
                    GIT_FACT_TIMEOUT_MS,
                ),
            ]
        );

        let mut invalid = GitRunner {
            outputs: [
                Ok("https://github.com/acme/repo.git".into()),
                Ok("not-a-git-object".into()),
            ]
            .into(),
            calls: Vec::new(),
        };
        let repository = resolve_artifact_repository(&mut invalid, root).unwrap();
        assert_eq!(
            resolve_artifact_expectation(&mut invalid, root, repository)
                .unwrap_err()
                .status,
            ArtifactVerificationStatus::Retryable
        );
    }

    #[test]
    fn artifact_expectation_accepts_each_exact_supervised_worktree_head() {
        let repository = GitHubRepository::from_name_with_owner("acme/repo").unwrap();
        let roots = [PathBuf::from("/tmp/root"), PathBuf::from("/tmp/delegated")];
        let mut runner = GitRunner {
            outputs: [
                Ok("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n".into()),
                Ok("BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\n".into()),
            ]
            .into(),
            calls: Vec::new(),
        };

        let expectation =
            resolve_artifact_expectation_for_worktrees(&mut runner, &roots, repository).unwrap();

        assert!(expectation.matches_head("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(expectation.matches_head("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"));
        assert_eq!(expectation.head_oids().count(), 2);
        assert_eq!(runner.calls.len(), 2);
        assert_eq!(runner.calls[0].1[1], "/tmp/root");
        assert_eq!(runner.calls[1].1[1], "/tmp/delegated");
    }

    #[test]
    fn workspace_git_fact_failures_are_retryable_and_effect_free() {
        let root = Path::new("/tmp/work tree;not-shell");
        for mut unavailable in [
            GitRunner {
                outputs: [Err(())].into(),
                calls: Vec::new(),
            },
            GitRunner {
                outputs: [Ok("not-a-github-remote".into())].into(),
                calls: Vec::new(),
            },
        ] {
            assert_eq!(
                resolve_artifact_repository(&mut unavailable, root)
                    .unwrap_err()
                    .status,
                ArtifactVerificationStatus::Retryable
            );
        }
        let mut revision_unavailable = GitRunner {
            outputs: [Err(())].into(),
            calls: Vec::new(),
        };
        assert_eq!(
            resolve_artifact_expectation(
                &mut revision_unavailable,
                root,
                usagi_core::domain::pr_inventory::GitHubRepository::from_name_with_owner(
                    "acme/repo",
                )
                .unwrap(),
            )
            .unwrap_err()
            .status,
            ArtifactVerificationStatus::Retryable
        );

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;

            let non_utf8 = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&[0xff]));
            let mut never_called = GitRunner {
                outputs: VecDeque::new(),
                calls: Vec::new(),
            };
            assert!(resolve_artifact_repository(&mut never_called, &non_utf8).is_err());
            assert!(
                resolve_artifact_expectation(
                    &mut never_called,
                    &non_utf8,
                    usagi_core::domain::pr_inventory::GitHubRepository::from_name_with_owner(
                        "acme/repo",
                    )
                    .unwrap(),
                )
                .is_err()
            );
            assert!(never_called.calls.is_empty());
        }
    }
}
