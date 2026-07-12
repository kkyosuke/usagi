//! `unite` — unite view の workspace 操作コマンドの IF。

use super::super::{CommandResult, Run};

/// `unite` のハンドラ。
pub(in crate::usecase::overview) struct Unite {
    pub(in crate::usecase::overview) arguments: String,
}

impl Run for Unite {
    fn run(&self) -> CommandResult {
        CommandResult::not_implemented("unite", &self.arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::super::render;
    use crate::usecase::overview::{Command, CommandResult};

    #[test]
    fn preserves_unite_arguments_in_the_stub_result() {
        let result = render(Command::Unite {
            arguments: "add backend".to_owned(),
        });
        assert_eq!(
            result,
            CommandResult::NotImplemented {
                command: "unite",
                arguments: "add backend".to_owned(),
            }
        );
    }
}
