//! `diff` — selected session の差分表示コマンドの IF。

use super::super::{CommandResult, Run};

/// `diff` のハンドラ。
pub(in crate::usecase::closeup) struct Diff {
    pub(in crate::usecase::closeup) arguments: String,
}

impl Run for Diff {
    fn run(&self) -> CommandResult {
        CommandResult::not_implemented("diff", &self.arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::super::render;
    use crate::usecase::closeup::{Command, CommandResult};

    #[test]
    fn preserves_diff_arguments_in_the_stub_result() {
        let result = render(Command::Diff {
            arguments: "--stat".to_owned(),
        });
        assert_eq!(
            result,
            CommandResult::NotImplemented {
                command: "diff",
                arguments: "--stat".to_owned(),
            }
        );
    }
}
