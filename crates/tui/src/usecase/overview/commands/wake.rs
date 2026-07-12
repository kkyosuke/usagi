//! `wake` — running agent への wake コマンドの IF。

use super::super::{CommandResult, Run};

/// `wake` のハンドラ。
pub(in crate::usecase::overview) struct Wake {
    pub(in crate::usecase::overview) arguments: String,
}

impl Run for Wake {
    fn run(&self) -> CommandResult {
        CommandResult::not_implemented("wake", &self.arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::super::render;
    use crate::usecase::overview::{Command, CommandResult};

    #[test]
    fn preserves_wake_arguments_in_the_stub_result() {
        let result = render(Command::Wake {
            arguments: "-i 30m".to_owned(),
        });
        assert_eq!(
            result,
            CommandResult::NotImplemented {
                command: "wake",
                arguments: "-i 30m".to_owned(),
            }
        );
    }
}
