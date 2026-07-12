//! `terminal` — selected session の terminal 起動コマンドの IF。

use super::super::{CommandResult, Run};

/// `terminal` のハンドラ。
pub(in crate::usecase::closeup) struct Terminal {
    pub(in crate::usecase::closeup) arguments: String,
}

impl Run for Terminal {
    fn run(&self) -> CommandResult {
        CommandResult::not_implemented("terminal", &self.arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::super::render;
    use crate::usecase::closeup::{Command, CommandResult};

    #[test]
    fn preserves_terminal_arguments_in_the_stub_result() {
        let result = render(Command::Terminal {
            arguments: "new".to_owned(),
        });
        assert_eq!(
            result,
            CommandResult::NotImplemented {
                command: "terminal",
                arguments: "new".to_owned(),
            }
        );
    }
}
