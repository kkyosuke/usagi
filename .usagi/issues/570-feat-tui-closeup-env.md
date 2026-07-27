---
number: 570
title: feat: TUI Closeup に env コマンドを追加
status: done
priority: medium
labels: [tui, closeup, env]
dependson: []
related: []
created_at: 2026-07-27T22:35:59.613038+00:00
updated_at: 2026-07-27T22:39:45.446002+00:00
---

## 目的

v2 TUI の Closeup command surface から、既存の環境変数 editor を開けるようにする。

## 受け入れ条件

- Closeup command registry に `env [workspace|global]` が登録される。
- Action / Prompt の両 modal mode から `env` を実行できる。
- 引数なしと `workspace` は workspace scope、`global` は global scope の editor を開く。
- 未知の scope は Closeup を閉じず安全な notice で拒否する。
- command metadata、controller、modal completion/selection のテストと v2 ドキュメントを更新する。
