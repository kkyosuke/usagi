//! `chat` — selected session の local LLM chat コマンドの IF。

use super::super::{CommandResult, Run};

/// `chat` のハンドラ。
pub(in crate::usecase::closeup) struct Chat {
    pub(in crate::usecase::closeup) arguments: String,
}

impl Run for Chat {
    fn run(&self) -> CommandResult {
        CommandResult::not_implemented("chat", &self.arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::super::render;
    use crate::usecase::closeup::{Command, CommandResult};

    #[test]
    fn preserves_chat_arguments_in_the_stub_result() {
        let result = render(Command::Chat {
            arguments: "model".to_owned(),
        });
        assert_eq!(
            result,
            CommandResult::NotImplemented {
                command: "chat",
                arguments: "model".to_owned(),
            }
        );
    }
}
