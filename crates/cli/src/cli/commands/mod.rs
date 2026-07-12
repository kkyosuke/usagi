//! 人間向け CLI サブコマンドのハンドラ置き場。1 サブコマンド（または関連する
//! サブコマンド群）を 1 モジュールにし、`cli/mod.rs` の dispatch から呼ぶ。
//!
//! 各ハンドラは presentation に徹する — 引数解析済みの入力を受け取り、store 系は
//! usagi-core の usecase を直接呼び、session 系は usagi-core の IPC クライアント経由で
//! daemon に委譲し、結果を整形して返す（独自のビジネスロジックは持たない）。
//! MCP の tool アダプタ（`crate::mcp::tools`）は同じ core usecase を呼ぶ兄弟である。
//! v2 では必要になった時点でハンドラを追加する。
