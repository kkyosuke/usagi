//! Overview コマンドの個別ハンドラ。
//!
//! **1 コマンド = 1 ファイル**とし、各ハンドラ型が [`super::Run`] を実装する。
//! [`super::Command::into_handler`] が解釈済みコマンドとの対応付けを 1 か所に集約する。

mod config;
mod env;
mod issue;
mod session;

pub(super) use config::Config;
pub(super) use env::Env;
pub(super) use issue::Issue;
pub(super) use session::Session;

#[cfg(test)]
pub(crate) fn render(command: super::Command) -> super::CommandResult {
    command.into_handler().run()
}
