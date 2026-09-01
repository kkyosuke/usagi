//! Independent verification for the Goal review-ready pull-request contract.
//!
//! Worker output is only a candidate. The verifier canonicalizes that candidate
//! and asks the provider through a fixed argv boundary before producing the
//! redaction-safe fact accepted by the supervisor reducer.

use sha2::{Digest, Sha256};
use usagi_core::domain::{
    agent::StructuredResult,
    pr_inventory::{PrChecksState, PrState, canonicalize},
    supervisor::GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
};

use super::{
    pr_inventory::{GhProcessPort, gh_pr_view_argv, parse_gh_pr_view},
    supervisor_runtime::{ArtifactVerification, ArtifactVerifier},
};

const VERIFY_TIMEOUT_MS: u64 = 5_000;

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
        passed: false,
        result_digest: evidence_digest(reason),
        safe_summary: reason.into(),
    }
}

impl<R: GhProcessPort> ArtifactVerifier for GoalArtifactVerifier<R> {
    fn verify(
        &mut self,
        contract: &str,
        result: Option<&StructuredResult>,
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
        let Ok(output) = self
            .runner
            .run("gh", &gh_pr_view_argv(&identity), VERIFY_TIMEOUT_MS)
        else {
            return rejected("pull request verification provider is unavailable");
        };
        let Some(view) = parse_gh_pr_view(&output) else {
            return rejected("pull request verification returned an invalid response");
        };
        if view.state != PrState::Open {
            return rejected("pull request is not open");
        }
        if view.draft {
            return rejected("pull request is still a draft");
        }
        if matches!(
            view.checks,
            Some(PrChecksState::Failing | PrChecksState::Pending)
        ) {
            return rejected("pull request checks are not passing");
        }
        // Failing and pending returned above. Keep the accepted-state mapping
        // closed over only the two values which can actually reach evidence.
        let (checks, safe_summary) = if view.checks == Some(PrChecksState::Passing) {
            (
                "passing",
                "open review-ready pull request verified with passing checks",
            )
        } else {
            (
                "not_configured",
                "open review-ready pull request verified with no configured checks",
            )
        };
        ArtifactVerification {
            passed: true,
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

    #[test]
    fn only_an_open_non_draft_pr_with_passing_checks_satisfies_the_contract() {
        let cases = [
            (view("OPEN", false, r#"[{"conclusion":"SUCCESS"}]"#), true),
            (view("OPEN", false, "[]"), true),
            (view("OPEN", true, r#"[{"conclusion":"SUCCESS"}]"#), false),
            (
                view("CLOSED", false, r#"[{"conclusion":"SUCCESS"}]"#),
                false,
            ),
            (view("OPEN", false, r#"[{"conclusion":"PENDING"}]"#), false),
            (view("OPEN", false, r#"[{"conclusion":"FAILURE"}]"#), false),
        ];
        for (provider, expected) in cases {
            let mut verifier = GoalArtifactVerifier::new(Runner([Ok(provider)].into()));
            assert_eq!(
                verifier
                    .verify(
                        GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
                        Some(&result(Some("https://github.com/acme/repo/pull/42")))
                    )
                    .passed,
                expected
            );
        }
    }

    #[test]
    fn missing_invalid_unsupported_and_provider_failures_are_safe_rejections() {
        let mut unavailable = GoalArtifactVerifier::new(Runner([Err(())].into()));
        assert!(
            !unavailable
                .verify(
                    GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
                    Some(&result(Some("https://github.com/acme/repo/pull/42")))
                )
                .passed
        );
        let mut malformed = GoalArtifactVerifier::new(Runner([Ok("not json".into())].into()));
        let malformed = malformed.verify(
            GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
            Some(&result(Some("https://github.com/acme/repo/pull/42"))),
        );
        assert!(!malformed.passed);
        assert_eq!(
            malformed.safe_summary,
            "pull request verification returned an invalid response"
        );
        let mut never_called = GoalArtifactVerifier::new(Runner(VecDeque::new()));
        for (contract, result) in [
            (GOAL_REVIEW_READY_ARTIFACT_CONTRACT, result(None)),
            (
                GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
                result(Some("not a pr")),
            ),
            (
                "unknown",
                result(Some("https://github.com/acme/repo/pull/42")),
            ),
        ] {
            assert!(!never_called.verify(contract, Some(&result)).passed);
        }
    }
}
