//! `preview` — Markdown preview コマンドの IF。

use super::super::{CommandResult, Run};

/// `preview` のハンドラ。
pub(in crate::usecase::overview) struct Preview {
    pub(in crate::usecase::overview) arguments: String,
}

impl Run for Preview {
    fn run(&self) -> CommandResult {
        CommandResult::not_implemented("preview", &self.arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::super::render;
    use crate::usecase::overview::{Command, CommandResult};

    #[test]
    fn preserves_preview_arguments_in_the_stub_result() {
        let result = render(Command::Preview {
            arguments: "README".to_owned(),
        });
        assert_eq!(
            result,
            CommandResult::NotImplemented {
                command: "preview",
                arguments: "README".to_owned(),
            }
        );
    }
}
