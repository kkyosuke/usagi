//! `config` — workspace local settings 編集コマンドの IF。

use super::super::{CommandResult, Run};

/// `config` のハンドラ。
pub(in crate::usecase::overview) struct Config {
    pub(in crate::usecase::overview) arguments: String,
}

impl Run for Config {
    fn run(&self) -> CommandResult {
        CommandResult::not_implemented("config", &self.arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::super::render;
    use crate::usecase::overview::{Command, CommandResult};

    #[test]
    fn preserves_config_arguments_in_the_stub_result() {
        let result = render(Command::Config {
            arguments: "profile".to_owned(),
        });
        assert_eq!(
            result,
            CommandResult::NotImplemented {
                command: "config",
                arguments: "profile".to_owned(),
            }
        );
    }
}
