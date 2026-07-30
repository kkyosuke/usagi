---
number: 590
title: docs(conventions): 依存クレート表に toml/shell-words/portable-pty を追記する
status: done
priority: low
labels: [docs, conventions]
dependson: []
related: []
created_at: 2026-07-30T10:48:16.457732+00:00
updated_at: 2026-07-30T23:20:07.069946+00:00
---

## 背景

`document/06-conventions.md` の「依存クレート」節（本節が SSoT と明記）が持つ「現在追加済みの外部依存」表に対し、実際の Cargo.toml は以下の3クレートを追加済みだが表に記載されていない。

- `toml`（`Cargo.toml:83`、`crates/core/Cargo.toml:27` で `usagi-core` が使用）
- `shell-words`（`Cargo.toml:108`、`crates/core/Cargo.toml:29` で `usagi-core` が使用）
- `portable-pty`（`Cargo.toml:116`、`crates/daemon/Cargo.toml:20` で `usagi-daemon` が使用）

`document/06-conventions.md#ドキュメント規約` 自身が「実装を変えたら同じ PR で対応ドキュメントも更新する」と定めており、この3クレート追加時にこの規約が守られていなかった。

## 対象

- 依存クレート表に `toml` / `shell-words` / `portable-pty` の行を追加し、使途・種別（本依存/dev/build依存）・使用箇所（`usagi-core` の domain には持ち込まない前提を含む）を明記する。
- 追記にあたり、各クレートが実際にどのモジュールで使われているか（例: `toml` は `usagi-core` のどの設定/config 解析、`shell-words` は Bash 字句解析、`portable-pty` は daemon の PTY）を実装を確認して正確に記述する。

## 受入条件

- [ ] `document/06-conventions.md` の依存クレート表に `toml` / `shell-words` / `portable-pty` の行が追加されている。
- [ ] `lychee` によるリンク・アンカー検証が green（Markdown 差分のみのため他の Rust gate は不要）。
