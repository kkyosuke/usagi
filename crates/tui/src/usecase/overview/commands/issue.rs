//! `issue` — workspace issue 閲覧コマンドの IF。

use super::super::{CommandResult, Run};

/// `issue` のハンドラ。
pub(in crate::usecase::overview) struct Issue {
    pub(in crate::usecase::overview) arguments: String,
}

impl Run for Issue {
    fn run(&self) -> CommandResult {
        CommandResult::not_implemented("issue", &self.arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::super::render;
    use crate::usecase::overview::{Command, CommandResult};

    #[test]
    fn preserves_issue_arguments_in_the_stub_result() {
        let result = render(Command::Issue {
            arguments: "show 3".to_owned(),
        });
        assert_eq!(
            result,
            CommandResult::NotImplemented {
                command: "issue",
                arguments: "show 3".to_owned(),
            }
        );
    }
}
