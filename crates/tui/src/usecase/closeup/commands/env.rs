//! `env` — workspace environment settings 編集コマンドの IF。

use super::super::{CommandResult, Run};

/// `env` のハンドラ。
pub(in crate::usecase::closeup) struct Env {
    pub(in crate::usecase::closeup) arguments: String,
}

impl Run for Env {
    fn run(&self) -> CommandResult {
        CommandResult::not_implemented("env", &self.arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::super::render;
    use crate::usecase::closeup::{Command, CommandResult};

    #[test]
    fn preserves_env_arguments_in_the_stub_result() {
        let result = render(Command::Env {
            arguments: "global".to_owned(),
        });
        assert_eq!(
            result,
            CommandResult::NotImplemented {
                command: "env",
                arguments: "global".to_owned(),
            }
        );
    }
}
