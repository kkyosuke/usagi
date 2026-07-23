//! 合成ルートの実行時 adapter 群。
//!
//! ここには OS・端末・プロセスなどの実 IO だけを置く。各 adapter はライブラリ
//! クレートが定義する port を実装し、画面・CLI・daemon の面どうしは依存させない。

pub(crate) mod agent_tab_intent;
pub(crate) mod bootstrap;
pub(crate) mod cli;
pub(crate) mod clipboard;
pub(crate) mod daemon;
pub(crate) mod launchd;
pub(crate) mod tui;
