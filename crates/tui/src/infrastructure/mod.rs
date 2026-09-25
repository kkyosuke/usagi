//! TUI 面ローカルの infrastructure 層。
//!
//! daemon の reply を TUI 語彙へ翻訳する adapter（[`daemon_reply`]）と、実端末
//! backend が出した live 入力を `Key` へ分類する adapter（[`live_input`]）を置く。
//! wire 契約そのものは usagi-core の IPC protocol module が持ち、接続・lane・
//! thread・端末 backend の所有は合成ルートに残る。この層は注入された payload と
//! 入力だけを見る純粋な変換なので、依存注入なしでそのまま単体テストできる。

pub mod daemon_reply;
pub mod live_input;
