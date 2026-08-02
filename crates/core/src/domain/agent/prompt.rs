//! Product-neutral system-prompt text for agent launches.
//!
//! This module is the single source of truth for text injected by product
//! adapters. It deliberately does not own adapter CLI syntax or launch-scope
//! resolution.

use crate::domain::role::RoleId;

const ROOT_PROMPT: &str = "<context>\nあなたは usagi が管理するワークスペースの root ディレクトリ（統括環境）で起動されています。\n</context>\n<instructions>\n受け取った指示や issue をもとに、どのようなタスクを各セッションに実行させるべきかを判別してください。\n</instructions>";

const SESSION_WORKTREE_PROMPT: &str = "<context>\nあなたは usagi が管理するセッション専用の worktree 内で起動されています。このディレクトリは既に独立した作業環境のため、新たに git worktree を作成する必要はありません。\n</context>\n<constraints>\n- 作業はこのディレクトリ配下だけで完結させてください。\n- 親ディレクトリ（メインリポジトリ本体）のファイルは読み書きしないでください。\n- 親ディレクトリへ cd しないでください。\n</constraints>\n<instructions>\n受けた指示を実行して、何かしらの結果（設計やPRなど）みれる形で提供してください。\n</instructions>";

const LOCAL_LLM_PROMPT: &str = "<delegation_instructions>\nトークン節約のため、要約・命名・定型文の生成・単純な変換といった軽量で重要度の低いタスクは、MCP ツール local_llm_ask（ローカル LLM）に委譲してください。判断が必要な作業や重要な実装はあなた自身が行ってください。\n</delegation_instructions>";

/// The system-prompt text for a coordinator in the main checkout.
#[must_use]
pub const fn root_prompt() -> &'static str {
    ROOT_PROMPT
}

/// The system-prompt text for an agent in a managed session worktree.
#[must_use]
pub const fn session_worktree_prompt() -> &'static str {
    SESSION_WORKTREE_PROMPT
}

/// The system-prompt suffix used when trusted local-LLM delegation is enabled.
#[must_use]
pub const fn local_llm_delegation_prompt() -> &'static str {
    LOCAL_LLM_PROMPT
}

/// Selects the root or session prompt and optionally appends the trusted
/// local-LLM delegation instruction.
///
/// Callers derive `is_root` from `LaunchRequest.scope.session_id.is_none()`.
#[must_use]
pub fn session_system_prompt(is_root: bool, local_llm_delegation: bool) -> String {
    session_system_prompt_with_role(is_root, None, local_llm_delegation)
}

/// Composes the immutable scope boundary, one optional effective role policy,
/// and the trusted local-LLM suffix exactly once and in that order.
#[must_use]
pub fn session_system_prompt_with_role(
    is_root: bool,
    role: Option<(&RoleId, &str)>,
    local_llm_delegation: bool,
) -> String {
    let base = if is_root {
        root_prompt()
    } else {
        session_worktree_prompt()
    };
    let mut prompt = base.to_owned();
    if let Some((id, instructions)) = role {
        prompt.push_str("\n<role id=\"");
        prompt.push_str(id.as_str());
        prompt.push_str("\">\n");
        prompt.push_str(instructions);
        prompt.push_str("\n</role>");
    }
    if local_llm_delegation {
        prompt.push('\n');
        prompt.push_str(local_llm_delegation_prompt());
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1_ROOT_PROMPT: &str = "<context>\nあなたは usagi が管理するワークスペースの root ディレクトリ（統括環境）で起動されています。\n</context>\n<instructions>\n受け取った指示や issue をもとに、どのようなタスクを各セッションに実行させるべきかを判別してください。\n</instructions>";
    const V1_SESSION_WORKTREE_PROMPT: &str = "<context>\nあなたは usagi が管理するセッション専用の worktree 内で起動されています。このディレクトリは既に独立した作業環境のため、新たに git worktree を作成する必要はありません。\n</context>\n<constraints>\n- 作業はこのディレクトリ配下だけで完結させてください。\n- 親ディレクトリ（メインリポジトリ本体）のファイルは読み書きしないでください。\n- 親ディレクトリへ cd しないでください。\n</constraints>\n<instructions>\n受けた指示を実行して、何かしらの結果（設計やPRなど）みれる形で提供してください。\n</instructions>";
    const V1_LOCAL_LLM_PROMPT: &str = "<delegation_instructions>\nトークン節約のため、要約・命名・定型文の生成・単純な変換といった軽量で重要度の低いタスクは、MCP ツール local_llm_ask（ローカル LLM）に委譲してください。判断が必要な作業や重要な実装はあなた自身が行ってください。\n</delegation_instructions>";

    #[test]
    fn prompt_fragments_are_byte_identical_to_v1() {
        assert_eq!(root_prompt().as_bytes(), V1_ROOT_PROMPT.as_bytes());
        assert_eq!(
            session_worktree_prompt().as_bytes(),
            V1_SESSION_WORKTREE_PROMPT.as_bytes()
        );
        assert_eq!(
            local_llm_delegation_prompt().as_bytes(),
            V1_LOCAL_LLM_PROMPT.as_bytes()
        );
    }

    #[test]
    fn session_system_prompt_composes_all_scope_and_delegation_variants() {
        assert_eq!(session_system_prompt(true, false), V1_ROOT_PROMPT);
        assert_eq!(
            session_system_prompt(false, false),
            V1_SESSION_WORKTREE_PROMPT
        );
        assert_eq!(
            session_system_prompt(true, true),
            format!("{V1_ROOT_PROMPT}\n{V1_LOCAL_LLM_PROMPT}")
        );
        assert_eq!(
            session_system_prompt(false, true),
            format!("{V1_SESSION_WORKTREE_PROMPT}\n{V1_LOCAL_LLM_PROMPT}")
        );
    }

    #[test]
    fn effective_role_is_bounded_between_scope_and_optional_suffix_once() {
        let id = RoleId::new("reviewer").unwrap();
        let prompt =
            session_system_prompt_with_role(false, Some((&id, "Review correctness.")), true);
        assert!(prompt.starts_with(V1_SESSION_WORKTREE_PROMPT));
        assert!(prompt.contains("<role id=\"reviewer\">\nReview correctness.\n</role>"));
        assert!(prompt.ends_with(V1_LOCAL_LLM_PROMPT));
        assert_eq!(prompt.matches("<role id=").count(), 1);
        assert_eq!(prompt.matches("<delegation_instructions>").count(), 1);
    }
}
