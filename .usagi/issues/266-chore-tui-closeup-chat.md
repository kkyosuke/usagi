---
number: 266
title: chore(tui): Closeup の未実装 chat 操作を削除する
status: todo
priority: high
labels: [chore, tui]
dependson: []
related: [143]
created_at: 2026-07-13T00:17:05.050534+00:00
updated_at: 2026-07-13T00:17:05.050534+00:00
---

## 目的

v2 TUI の Closeup から未実装の `chat` 操作を完全に削除する。terminal / agent / close / diff の既存動作は維持する。

## 背景

`chat` は daemon / IPC / API に接続されていない `NotImplemented` スタブであるにもかかわらず、Closeup の command registry、アクションモーダル、controller dispatch、テスト、実装済み仕様ドキュメントに現れる。利用者に実行不能な操作を提示せず、死コードを残さない。

調査時点で daemon・core・CLI API に `chat` 語彙は存在しないため、変更範囲は `crates/tui` と v2 仕様ドキュメントに限られる。

## 変更方針

- `crates/tui/src/usecase/closeup/commands/chat.rs` を削除し、`commands/mod.rs` の module / export を除去する。
- `closeup::Command` から `Chat` variant を、private registry から metadata / factory を、`name` / `into_handler` から対応分岐を除去する。
- registry の完全性・parse / dispatch テストを 4 コマンド（`agent` / `close` / `diff` / `terminal`）へ更新し、`chat` は未知コマンドとして拒否されることを検証する。
- `submit_closeup` の `Chat` 分岐を除去する。未接続の `diff` は従来どおり unavailable とし、`terminal` / `agent` の effect と `close` の target / `--force` 検証を変更しない。
- registry を表示源にする `CloseupModal` を 4 action 前提へ更新し、選択の循環・submission・描画テストから `chat` を除去する。
- `document/02-architecture.md` の Closeup command vocabulary を実装に一致させる。
- 後続の #143 が古い action 集合を前提にしないよう、必要ならその本文から `chat` 前提を削除する。

## 受け入れ条件

- Closeup の画面表示・キー操作で `chat` が候補として現れない。
- `interpret("chat")` は unknown command を返し、`Chat` 型・handler module・controller branch は存在しない。
- Closeup modal は `agent` / `close` / `diff` / `terminal` のみを registry 順に表示し、上下移動と Enter の submission が正しく循環する。
- terminal / agent は従来の `OpenTerminal` / `OpenAgent` effect を返し、session の close と diff unavailable の既存挙動を維持する。
- daemon / IPC / API に chat 専用の dead code を追加せず、残存参照がない。
- 実装済み仕様ドキュメントの command vocabulary が実装と一致する。

## テスト方針

- `cargo test -p usagi-tui closeup`
- `cargo test -p usagi-tui controller`
- `cargo fmt --all -- --check`
- Rust 差分を含むため、PR 前に `cargo clippy --workspace --all-targets -- -D warnings` と coverage 100% gate を実行する。
- Markdown 差分を含むため、`lychee --config lychee.toml --no-progress '*.md' 'document/**/*.md' 'v1/README.md' 'v1/document/**/*.md' '.agents/**/*.md' '.github/**/*.md'` を実行する。

## 非目標

- `diff` の実装や availability を変更しない。
- Closeup 以外のチャット機能を追加・削除しない。
- daemon、IPC、API の機能変更を行わない。
