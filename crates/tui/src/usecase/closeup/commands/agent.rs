//! `agent` — selected session の Agent 起動コマンドの IF。

use super::super::{CommandResult, Run};

/// `agent` のハンドラ。
pub(in crate::usecase::closeup) struct Agent {
    pub(in crate::usecase::closeup) arguments: String,
}

impl Run for Agent {
    fn run(&self) -> CommandResult {
        CommandResult::not_implemented("agent", &self.arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::super::render;
    use crate::usecase::closeup::{Command, CommandResult};

    #[test]
    fn preserves_agent_arguments_in_the_stub_result() {
        let result = render(Command::Agent {
            arguments: "codex".to_owned(),
        });
        assert_eq!(
            result,
            CommandResult::NotImplemented {
                command: "agent",
                arguments: "codex".to_owned(),
            }
        );
    }
}
