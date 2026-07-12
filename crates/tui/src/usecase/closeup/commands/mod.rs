//! Closeup コマンドの個別ハンドラ。
//!
//! **1 コマンド = 1 ファイル**とし、各ハンドラ型が [`super::Run`] を実装する。
//! [`super::Command::into_handler`] が解釈済みコマンドとの対応付けを 1 か所に集約する。

mod agent;
mod chat;
mod close;
mod diff;
mod terminal;

pub(super) use agent::Agent;
pub(super) use chat::Chat;
pub(super) use close::Close;
pub(super) use diff::Diff;
pub(super) use terminal::Terminal;

#[cfg(test)]
pub(crate) fn render(command: super::Command) -> super::CommandResult {
    command.into_handler().run()
}
