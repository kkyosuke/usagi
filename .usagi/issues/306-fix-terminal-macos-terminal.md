---
number: 306
title: fix(terminal): macOS の対話 terminal と同等の起動環境を再現する
status: todo
priority: high
labels: [terminal, macos, pty, parity]
dependson: []
related: [218, 264]
created_at: 2026-07-15T00:28:01.613067+00:00
updated_at: 2026-07-15T00:29:55.022478+00:00
---

## 目的

macOS で usagi の terminal tab から起動する shell / Agent が、利用者が通常の Terminal.app 等から起動した対話 terminal と同等に必要な環境・端末特性を得られるようにする。

## 調査・設計範囲

- login / interactive shell、`SHELL`、`TERM`、locale、PATH と working directory の実効値を、秘密情報を収集・永続化せず比較可能な形で整理する。
- macOS の PTY・process group・resize・signal・UTF-8 / wide character・clipboard escape sequence の互換性を確認する。
- Linux / Windows の既存 profile と共通化できる契約は platform-neutral に置き、macOS 固有処理は adapter に閉じ込める。

## 受け入れ条件

- macOS 実機または macOS CI で、shell discovery・interactive startup・working directory・resize・exit / detach を検証する。
- `TERM` / locale / PATH の扱いを明文化し、ユーザーの shell 設定を不必要に上書きしない。
- プロファイル解決は testable な純粋ロジックに分離し、実 PTY の検証を最小の E2E で補う。
- 既存の daemon-owned terminal ownership / IPC 契約を後退させない。
