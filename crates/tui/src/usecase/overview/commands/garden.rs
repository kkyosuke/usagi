//! `garden` — session garden screen saver を手動で開く IF。

use super::super::{CommandResult, Run};

/// `garden` のハンドラ。
pub(in crate::usecase::overview) struct Garden {
    pub(in crate::usecase::overview) arguments: String,
}

impl Run for Garden {
    fn run(&self) -> CommandResult {
        CommandResult::not_implemented("garden", &self.arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::super::render;
    use crate::usecase::overview::{Command, CommandResult};

    #[test]
    fn preserves_garden_arguments_in_the_stub_result() {
        let result = render(Command::Garden {
            arguments: "extra".to_owned(),
        });
        assert_eq!(
            result,
            CommandResult::NotImplemented {
                command: "garden",
                arguments: "extra".to_owned(),
            }
        );
    }
}
