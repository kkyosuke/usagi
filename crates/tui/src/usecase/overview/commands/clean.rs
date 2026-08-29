//! `clean` — daemon-owned orphan session resource cleanup command.

use super::super::{CommandResult, Run};

pub(in crate::usecase::overview) struct Clean {
    pub(in crate::usecase::overview) arguments: String,
}

impl Run for Clean {
    fn run(&self) -> CommandResult {
        CommandResult::not_implemented("clean", &self.arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::super::render;
    use crate::usecase::overview::{Command, CommandResult};

    #[test]
    fn preserves_clean_arguments_in_the_stub_result() {
        assert_eq!(
            render(Command::Clean {
                arguments: "--apply".to_owned(),
            }),
            CommandResult::NotImplemented {
                command: "clean",
                arguments: "--apply".to_owned(),
            }
        );
    }
}
