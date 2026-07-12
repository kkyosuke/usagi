//! `usagi agent-phase <phase>` — エージェントのライフサイクル phase を記録する内部コマンド。
//!
//! usagi がエージェント起動時に Claude の Stop フックへ配線し、フックが phase（例: `ended`）を
//! 渡して呼ぶ。人手で叩くものではない（`--help` 非表示）。フックは終了コードだけを見るため、
//! 標準出力には何も書かず、正常終了する。
//!
//! 現状は枠だけで、phase の実記録（daemon への報告）は未実装。受け取った phase を破棄して
//! 正常終了することで、フック配線が壊れないようにする。

use std::io::{self, Write};

use crate::cli::{Run, RunOutcome};

/// `usagi agent-phase <phase>` のハンドラ。
pub struct AgentPhase {
    pub phase: String,
}

impl Run for AgentPhase {
    fn run(&self, _out: &mut dyn Write) -> io::Result<RunOutcome> {
        // フックは終了コードだけを見る。phase の記録は未実装のため、いまは黙って成功する。
        let _ = &self.phase;
        Ok(RunOutcome::Exit(0))
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::execute;
    use crate::cli::{Command, RunOutcome};

    #[test]
    fn succeeds_silently() {
        let (outcome, output) = execute(Command::AgentPhase {
            phase: "ended".into(),
        });
        assert_eq!(outcome, RunOutcome::Exit(0));
        assert!(output.is_empty());
    }
}
