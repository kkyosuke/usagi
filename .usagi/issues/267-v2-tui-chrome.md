---
number: 267
title: v2 TUI右ペインにChrome風タブと空状態を実装
status: done
priority: medium
labels: [tui, v2]
dependson: []
related: []
created_at: 2026-07-13T00:23:46.201143+00:00
updated_at: 2026-07-13T00:32:16.107116+00:00
---

## 目的
v1 の右ペイン tab chrome と idle rabbit を参照し、v2 HomeProjection の簡易 `[]` strip を純粋な widget/layout に置き換える。

## 受け入れ条件
- `PaneState` が所有する pending/live の stable identity と選択を、表示名・index に還元せず chip の選択表示へ投影する。
- active tab は Chrome 風の accent chip と直下 underline で表し、狭い幅でも ANSI を壊さず表示幅内に clipping する。
- tab がない場合、右ペイン本文は静的うさぎと案内文を中央表示する。tick が変わっても静止状態は変わらない。
- modal overlay は既存の base-frame 合成を維持し、chrome/empty state が modal 背景として正しく残る。
- terminal/agent launch や runtime 接続は変更しない。

## 範囲
`crates/tui/src/presentation/views/workspace.rs` と純粋 widget/layout、および対応テスト・実装済み仕様ドキュメント。
