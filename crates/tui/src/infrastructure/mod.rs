//! TUI 面ローカルの infrastructure 層。daemon が所有する端末へ attach する
//! IPC クライアント側と、端末バックエンド（raw mode・差分描画のための端末制御・
//! キー/ホイール読み取り・クリップボード）を置く。daemon との通信で使う IPC
//! プロトコル型の定義は usagi-core が持ち、ここはそのクライアント実装だけを担う。
//! 実 IO は合成ルートから注入し、この層は依存注入でユニットテスト可能に保つ。

/// daemon push を TUI-local projection へ写す adapter。
pub mod daemon;
