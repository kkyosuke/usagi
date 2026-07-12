//! `usagi guard-workspace` — worktree の外へ出るツール呼び出しを拒否する内部コマンド。
//!
//! usagi がエージェント起動時に Claude の `PreToolUse` フックへ配線し、フックがツール入力を
//! stdin で渡して呼ぶ。終了コード 0 で許可、非 0 で拒否する。人手で叩くものではない
//! （`--help` 非表示）。
//!
//! 現状は枠だけで、パス検査（対象パスが worktree 内かの判定）は未実装。フック配線が
//! 壊れないよう、いまは常に許可（正常終了）する。**まだ enforcing ではない**点に注意
//! （実際の拒否ロジックは core usecase 実装時に入れる）。

use std::io::{self, Write};

use crate::cli::{Run, RunOutcome};

/// `usagi guard-workspace` のハンドラ。
pub struct GuardWorkspace;

impl Run for GuardWorkspace {
    fn run(&self, _out: &mut dyn Write) -> io::Result<RunOutcome> {
        // パス検査は未実装。いまは常に許可（終了コード 0）してフック配線を通す。
        Ok(RunOutcome::Exit(0))
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::execute;
    use crate::cli::{Command, RunOutcome};

    #[test]
    fn allows_by_succeeding_silently() {
        let (outcome, output) = execute(Command::GuardWorkspace);
        assert_eq!(outcome, RunOutcome::Exit(0));
        assert!(output.is_empty());
    }
}
