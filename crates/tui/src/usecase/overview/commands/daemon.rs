//! `daemon` — workspace daemon status surface の IF。

use super::super::{CommandResult, Run};

/// `daemon` のハンドラ。
pub(in crate::usecase::overview) struct Daemon {
    pub(in crate::usecase::overview) arguments: String,
}

impl Run for Daemon {
    fn run(&self) -> CommandResult {
        CommandResult::not_implemented("daemon", &self.arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::super::render;
    use crate::usecase::overview::{Command, CommandResult};

    #[test]
    fn preserves_daemon_arguments_in_the_stub_result() {
        let result = render(Command::Daemon {
            arguments: "extra".to_owned(),
        });
        assert_eq!(
            result,
            CommandResult::NotImplemented {
                command: "daemon",
                arguments: "extra".to_owned(),
            }
        );
    }
}
