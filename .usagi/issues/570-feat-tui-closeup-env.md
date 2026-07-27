---
number: 570
title: feat: TUI Closeup に env コマンドを追加
status: done
priority: medium
labels: [tui, closeup, env]
dependson: []
related: []
created_at: 2026-07-27T22:35:59.613038+00:00
updated_at: 2026-07-27T22:55:20.186894+00:00
---

## 目的

v2 TUI の Closeup command surface から、既存の workspace 環境変数 editor を開けるようにする。

## 受け入れ条件

- Closeup command registry に引数なしの `env` が登録される。
- Action / Prompt の両 modal mode から `env` を実行できる。
- Closeup から開く editor は workspace scope に固定し、global scope への切り替えを表示・受理しない。
- `env global` など引数付きの Closeup 入力は Closeup を閉じず安全な notice で拒否する。
- Overview の `env [workspace|global]` は従来どおり両 scope を編集できる。
- command metadata、controller、modal/editor rendering のテストと v2 ドキュメントを更新する。
