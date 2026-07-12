//! `close` — selected session の削除コマンドの IF。

use super::super::{CommandResult, Run};

/// `close` のハンドラ。
pub(in crate::usecase::closeup) struct Close {
    pub(in crate::usecase::closeup) arguments: String,
}

impl Run for Close {
    fn run(&self) -> CommandResult {
        CommandResult::not_implemented("close", &self.arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::super::render;
    use crate::usecase::closeup::{Command, CommandResult};

    #[test]
    fn preserves_close_arguments_in_the_stub_result() {
        let result = render(Command::Close {
            arguments: "--force".to_owned(),
        });
        assert_eq!(
            result,
            CommandResult::NotImplemented {
                command: "close",
                arguments: "--force".to_owned(),
            }
        );
    }
}
