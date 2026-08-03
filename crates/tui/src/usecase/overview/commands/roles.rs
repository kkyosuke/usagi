use super::super::{CommandResult, Run};

pub struct Roles {
    pub arguments: String,
}

impl Run for Roles {
    fn run(&self) -> CommandResult {
        CommandResult::not_implemented("roles", &self.arguments)
    }
}
