//! `session` — workspace session 操作コマンドの IF。

use super::super::{CommandResult, Run};

/// `session` のハンドラ。
pub(in crate::usecase::overview) struct Session {
    pub(in crate::usecase::overview) arguments: String,
}

impl Run for Session {
    fn run(&self) -> CommandResult {
        CommandResult::not_implemented("session", &self.arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::super::render;
    use crate::usecase::overview::{Command, CommandResult};

    #[test]
    fn preserves_session_arguments_in_the_stub_result() {
        let result = render(Command::Session {
            arguments: "create feature-x".to_owned(),
        });
        assert_eq!(
            result,
            CommandResult::NotImplemented {
                command: "session",
                arguments: "create feature-x".to_owned(),
            }
        );
    }
}
