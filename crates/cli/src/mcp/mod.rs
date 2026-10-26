//! MCP サーバ（`usagi mcp`）の presentation。エージェント向けの tool 面（IF）を持つ。
//! `tools` の [`tool::ToolDescriptor`] registry が metadata、schema、execution route、
//! caller policy を一つに束ね、dispatch は名前で descriptor を引いて schema を検証してから
//! route ごとの実行先へ送る。
//!
//! stdio 上の JSON-RPC 2.0 の serve ループ（`initialize` / `tools/list` / `tools/call`）は
//! [`serve`] が担う。issue / memory の Store route は接続時に固定した store root を core
//! usecase 経由で操作し、session / agent / terminal route は core IPC client を
//! 介して daemon-owned usecase へ委譲する。presentation 自身は business logic を所有しない。

pub mod protocol;
pub mod resources;
pub mod runtime_model;
pub mod serve;
pub mod tool;
pub mod tools;

pub use serve::{
    serve, serve_with_client, serve_with_client_and_caller, serve_with_client_and_caller_at,
    serve_with_client_and_caller_at_roots, serve_with_client_and_features,
    serve_with_client_and_snapshot,
};
