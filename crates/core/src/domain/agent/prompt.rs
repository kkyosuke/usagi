//! Product-neutral system-prompt text for agent launches.
//!
//! This module is the single source of truth for text injected by product
//! adapters. It deliberately does not own adapter CLI syntax or launch-scope
//! resolution.
//!
//! One launch composes at most three fragments, always in this order:
//!
//! ```text
//! scope   code-defined boundary of the checkout; a role can never replace it
//! tools   what the injected MCP server exposes; absent when none is wired
//! role    the effective, user-editable policy for this launch
//! ```
//!
//! The layering is deliberate. A scope fragment names no tool, so one tool
//! family is described in exactly one place, and a role can narrow the tools it
//! is told about because it is composed after them.

use crate::domain::agent::mcp_tools::McpToolFamilies;
use crate::domain::role::RoleId;

/// Which checkout a launch runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptScope {
    /// The coordinator in the workspace root checkout.
    Root,
    /// An agent isolated in a managed session worktree.
    Session,
}

const ROOT_SCOPE: &str = "<context>\nあなたは usagi が管理するワークスペースの root ディレクトリ（統括環境）で起動されています。\n</context>\n<instructions>\n受け取った指示をもとに、どのようなタスクを各セッションに実行させるべきかを判別してください。\n</instructions>";

const SESSION_SCOPE: &str = "<context>\nあなたは usagi が管理するセッション専用の worktree 内で起動されています。このディレクトリは既に独立した作業環境のため、新たに git worktree を作成する必要はありません。\n</context>\n<constraints>\n- 作業はこのディレクトリ配下だけで完結させてください。\n- 親ディレクトリ（メインリポジトリ本体）のファイルは読み書きしないでください。\n- 親ディレクトリへ cd しないでください。\n</constraints>\n<instructions>\n受けた指示を実行して、何かしらの結果（設計やPRなど）みれる形で提供してください。\n</instructions>";

const TOOLS_OPEN: &str = "<tools>\ntool 名と引数は tools/list のスキーマが正本です。";
const TOOLS_CLOSE: &str = "</tools>";

/// Every line must hold in both scopes, so a family never needs a root variant
/// and a session variant. The issue line states where writes are accepted rather
/// than whether *this* agent may write: in a session worktree that reads as the
/// permission, at the workspace root as the refusal, and both are true.
///
/// Session orchestration is never disabled, so its line is present whenever an
/// MCP server is wired. It carries the pointer to the guide resource instead of
/// the procedure itself, so the prompt does not restate what the guide owns.
const SESSION_TOOLS: &str = "- session: session の作成・観測・委譲・完了報告は daemon が権威です。手順は resource usagi://guides/orchestration を読んでください。";
const ISSUE_TOOLS: &str = "- issue: 作業の起点となる backlog を検索・参照できます。git 追跡下のため、書き込みは session worktree からだけ受理されます。";
const MEMORY_TOOLS: &str = "- memory: session をまたいで残す判断や制約を検索・保存できます。";
const LOCAL_LLM_TOOLS: &str = "- local_llm_ask: トークン節約のため、要約・命名・定型文の生成・単純な変換といった軽量で重要度の低いタスクは委譲してください。判断が必要な作業や重要な実装はあなた自身が行ってください。";

/// The immutable boundary of the checkout the launch runs in.
#[must_use]
pub const fn scope_prompt(scope: PromptScope) -> &'static str {
    match scope {
        PromptScope::Root => ROOT_SCOPE,
        PromptScope::Session => SESSION_SCOPE,
    }
}

/// Composes the scope boundary, the wired MCP tool families, and one optional
/// effective role policy exactly once and in that order.
#[must_use]
pub fn launch_system_prompt(
    scope: PromptScope,
    mcp: Option<McpToolFamilies>,
    role: Option<(&RoleId, &str)>,
) -> String {
    let mut prompt = scope_prompt(scope).to_owned();
    if let Some(families) = mcp {
        prompt.push('\n');
        prompt.push_str(TOOLS_OPEN);
        for line in tool_lines(families) {
            prompt.push('\n');
            prompt.push_str(line);
        }
        prompt.push('\n');
        prompt.push_str(TOOLS_CLOSE);
    }
    if let Some((id, instructions)) = role {
        prompt.push_str("\n<role id=\"");
        prompt.push_str(id.as_str());
        prompt.push_str("\">\n");
        prompt.push_str(instructions);
        prompt.push_str("\n</role>");
    }
    prompt
}

/// One line per available family, in a fixed order, so enabling a family adds a
/// line and disabling it removes that line and nothing else.
fn tool_lines(families: McpToolFamilies) -> impl Iterator<Item = &'static str> {
    [
        Some(SESSION_TOOLS),
        families.issue.then_some(ISSUE_TOOLS),
        families.memory.then_some(MEMORY_TOOLS),
        families.local_llm.then_some(LOCAL_LLM_TOOLS),
    ]
    .into_iter()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The session boundary is still byte-identical to v1. The root boundary
    /// deliberately diverges: v1 named the issue store there, and that fact now
    /// lives in the tools fragment, which knows whether the store is enabled.
    const V1_SESSION_SCOPE: &str = "<context>\nあなたは usagi が管理するセッション専用の worktree 内で起動されています。このディレクトリは既に独立した作業環境のため、新たに git worktree を作成する必要はありません。\n</context>\n<constraints>\n- 作業はこのディレクトリ配下だけで完結させてください。\n- 親ディレクトリ（メインリポジトリ本体）のファイルは読み書きしないでください。\n- 親ディレクトリへ cd しないでください。\n</constraints>\n<instructions>\n受けた指示を実行して、何かしらの結果（設計やPRなど）みれる形で提供してください。\n</instructions>";

    const ALL: McpToolFamilies = McpToolFamilies {
        issue: true,
        memory: true,
        local_llm: true,
    };
    const NONE: McpToolFamilies = McpToolFamilies {
        issue: false,
        memory: false,
        local_llm: false,
    };

    #[test]
    fn the_session_boundary_is_byte_identical_to_v1_and_names_no_tool() {
        assert_eq!(
            scope_prompt(PromptScope::Session).as_bytes(),
            V1_SESSION_SCOPE.as_bytes()
        );
        for scope in [PromptScope::Root, PromptScope::Session] {
            let boundary = scope_prompt(scope);
            for tool in ["issue", "memory", "tools/list", "local_llm_ask"] {
                assert!(
                    !boundary.contains(tool),
                    "{tool} leaked into the {scope:?} boundary"
                );
            }
        }
        assert_ne!(
            scope_prompt(PromptScope::Root),
            scope_prompt(PromptScope::Session)
        );
    }

    #[test]
    fn a_launch_without_an_mcp_server_gets_the_boundary_alone() {
        for scope in [PromptScope::Root, PromptScope::Session] {
            assert_eq!(launch_system_prompt(scope, None, None), scope_prompt(scope));
        }
    }

    #[test]
    fn each_family_contributes_exactly_its_own_line() {
        let baseline = launch_system_prompt(PromptScope::Session, Some(NONE), None);
        assert!(baseline.contains(SESSION_TOOLS));
        for (families, line) in [
            (
                McpToolFamilies {
                    issue: true,
                    ..NONE
                },
                ISSUE_TOOLS,
            ),
            (
                McpToolFamilies {
                    memory: true,
                    ..NONE
                },
                MEMORY_TOOLS,
            ),
            (
                McpToolFamilies {
                    local_llm: true,
                    ..NONE
                },
                LOCAL_LLM_TOOLS,
            ),
        ] {
            let prompt = launch_system_prompt(PromptScope::Session, Some(families), None);
            assert!(!baseline.contains(line), "{line} is not gated");
            assert_eq!(
                prompt.lines().count(),
                baseline.lines().count() + 1,
                "enabling one family changed more than one line"
            );
            assert_eq!(prompt.matches(line).count(), 1);
        }
    }

    #[test]
    fn the_tools_fragment_is_one_block_between_the_boundary_and_the_role() {
        let id = RoleId::new("reviewer").unwrap();
        let prompt = launch_system_prompt(
            PromptScope::Session,
            Some(ALL),
            Some((&id, "Review correctness.")),
        );

        assert!(prompt.starts_with(V1_SESSION_SCOPE));
        assert!(prompt.ends_with("<role id=\"reviewer\">\nReview correctness.\n</role>"));
        assert_eq!(prompt.matches("<tools>").count(), 1);
        assert_eq!(prompt.matches("</tools>").count(), 1);
        assert_eq!(prompt.matches("<role id=").count(), 1);

        let boundary = prompt.find(V1_SESSION_SCOPE).unwrap();
        let tools = prompt.find("<tools>").unwrap();
        let role = prompt.find("<role id=").unwrap();
        assert!(boundary < tools && tools < role);

        // Every enabled family appears once, in the declared order.
        let lines: Vec<usize> = [SESSION_TOOLS, ISSUE_TOOLS, MEMORY_TOOLS, LOCAL_LLM_TOOLS]
            .iter()
            .map(|line| {
                assert_eq!(prompt.matches(line).count(), 1);
                prompt.find(line).unwrap()
            })
            .collect();
        assert!(lines.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(lines.iter().all(|line| *line > tools && *line < role));
    }

    #[test]
    fn the_tools_fragment_does_not_branch_on_scope() {
        // Each line is written to hold in both scopes, so the same families
        // produce the same block. A scope-dependent claim would need two
        // variants per family and could disagree with the other scope.
        let root = launch_system_prompt(PromptScope::Root, Some(ALL), None);
        let session = launch_system_prompt(PromptScope::Session, Some(ALL), None);
        let block = |prompt: &str| prompt[prompt.find("<tools>").unwrap()..].to_owned();
        assert_eq!(block(&root), block(&session));
    }

    #[test]
    fn a_role_composes_without_an_mcp_server_too() {
        let id = RoleId::new("coder").unwrap();
        let prompt = launch_system_prompt(PromptScope::Root, None, Some((&id, "Implement.")));
        assert_eq!(
            prompt,
            format!("{ROOT_SCOPE}\n<role id=\"coder\">\nImplement.\n</role>")
        );
    }
}
