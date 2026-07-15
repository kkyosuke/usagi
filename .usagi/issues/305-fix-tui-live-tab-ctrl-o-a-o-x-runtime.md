---
number: 305
title: fix(tui): live tab の Ctrl+O a/o/x ショートカットを runtime まで接続する
status: todo
priority: high
labels: [tui, terminal, input, bug]
dependson: []
related: [279, 282]
created_at: 2026-07-15T00:28:01.578111+00:00
updated_at: 2026-07-15T00:29:54.983519+00:00
---

## 目的

live tab の Ctrl+O leader に続く `a`（action modal）、`o`（Switch）、`x`（選択 tab を閉じる）のショートカットを、classifier だけでなく実行時 effect まで一貫して動作させる。

## 背景

`LiveInputClassifier` には a/o/x の語彙がある一方、terminal / Agent tab を持つ実行経路で各 action が reducer・daemon operation・描画へ届くことを明示的に保証する必要がある。

## 受け入れ条件

- Ctrl+O a は live tab を壊さず Closeup action surface を開く。
- Ctrl+O o は Switch へ戻り、次の入力が PTY に送られない。
- Ctrl+O x は選択中 tab のみを閉じ、terminal の teardown / detach 方針と tab registry を矛盾させない。
- leader が無い通常の a/o/x は terminal input として一度だけ通過する。
- pending、tab 無し、最後の tab、agent tab、daemon error / reconnect の各ケースを安全に扱う。
- classifier → reducer/effect → runtime の統合テストを追加する。
