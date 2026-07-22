//! `reopen` — clear one continuation-scoped Agent dismissal.

use super::super::{CommandResult, Run};

pub(in crate::usecase::closeup) struct Reopen {
    pub(in crate::usecase::closeup) arguments: String,
}

impl Run for Reopen {
    fn run(&self) -> CommandResult {
        CommandResult::not_implemented("reopen", &self.arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::{Command, CommandResult};
    use super::super::render;

    #[test]
    fn preserves_reopen_continuation_in_the_stub_result() {
        assert_eq!(
            render(Command::Reopen {
                arguments: "continuation".to_owned(),
            }),
            CommandResult::NotImplemented {
                command: "reopen",
                arguments: "continuation".to_owned(),
            }
        );
    }
}
