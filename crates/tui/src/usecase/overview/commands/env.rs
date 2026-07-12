//! `env` — workspace environment settings 編集コマンドの IF。

use super::super::{CommandResult, Run};

/// `env` のハンドラ。
pub(in crate::usecase::overview) struct Env {
    pub(in crate::usecase::overview) arguments: String,
}

impl Run for Env {
    fn run(&self) -> CommandResult {
        CommandResult::not_implemented("env", &self.arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::super::render;
    use crate::usecase::overview::{Command, CommandResult};

    #[test]
    fn preserves_env_arguments_in_the_stub_result() {
        let result = render(Command::Env {
            arguments: "edit".to_owned(),
        });
        assert_eq!(
            result,
            CommandResult::NotImplemented {
                command: "env",
                arguments: "edit".to_owned(),
            }
        );
    }
}
