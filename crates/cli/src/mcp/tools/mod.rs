//! MCP tool アダプタの置き場。1 tool（または関連する tool 群）を 1 モジュールにし、
//! `mcp/mod.rs` の serve ループの dispatch から呼ぶ。
//!
//! 各アダプタは presentation に徹する — JSON-RPC の params を型に落とし、store 系は
//! usagi-core の usecase を直接呼び、session 系は usagi-core の IPC クライアント経由で
//! daemon に委譲し、結果を JSON-RPC 応答に整形する（独自のビジネスロジックは持たない）。
//! CLI のサブコマンドハンドラ（`crate::cli::commands`）は同じ core usecase を呼ぶ兄弟である。
//! v2 では必要になった時点で tool を追加する。
