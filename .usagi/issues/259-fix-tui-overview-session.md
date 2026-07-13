---
number: 259
title: fix(tui): Overview の session サブコマンド補完を行う
status: done
priority: high
labels: [tui, overview, session]
dependson: []
related: [226, 257]
created_at: 2026-07-13T00:08:58.484908+00:00
updated_at: 2026-07-13T00:14:33.453697+00:00
---

## 目的

Overview modal の Tab 補完がトップレベル command 名だけを置換するため、`session c` が `session` へ戻る。session command の登録済みサブコマンドを prefix 補完し、入力意図を維持する。

## スコープ

- `session c` を `session create` へ補完する。
- `session` の使用可能なサブコマンド（`create` / `list` / `overview` / `remove`）を command registry の metadata から導出し、表示 usage と補完候補を別管理にしない。
- 他のトップレベル command の既存 completion、候補選択、history、submit を変えない。
- 実際の session mutation / daemon IPC 接続は含めない（#260）。

## 受け入れ条件

- `session c` で Tab を押すと入力は `session create` になる。
- 空入力およびトップレベル prefix の Tab 補完は既存どおり selected command 名へ補完される。
- 未一致・曖昧な session subcommand は入力を破壊せず no-op とする。
- pure usecase / modal test と実装済み仕様 document を更新する。

## 依存・境界

- #226 の registry dispatch を拡張し、controller effect や daemon wire を増やさない。
- 実行可能な session command の typed effect / lifecycle adapter 接続は #260 が所有する。
