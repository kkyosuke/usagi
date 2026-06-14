---
number: 002
feature: workspace-screen
title: ワークスペース画面とコマンドモード基盤
status: todo
priority: high
category: tui
dependson: []
ref: usagi.ai doc/app/ui/layout.md, doc/app/ui/mode.md
---

# ワークスペース画面とコマンドモード基盤

## 概要

`usagi hop` 起動後の中核となるワークスペース操作画面を実装します。現状の TUI は welcome / home / new / open / config 画面までで、`document/01-overview.md` が説明する「左ペイン：worktree 一覧」「右ペイン：コマンド履歴」「下部：コマンド入力欄」のワークスペース画面が存在しません。この画面と、その上で動くコマンドモードの基盤が、後続の TUI 内コマンド（`session` / `space` / `ai` / `terminal` / `history` / `man`）すべての土台になります。

## やること

- ワークスペース画面のレイアウト（worktree 一覧ペイン / コマンド履歴ペイン / コマンド入力欄）を実装する。
- サイドメニューモードとコマンドモードの 2 モードと、その切り替えを実装する。
- コマンドモードに Tab 補完と履歴遡り（↑/↓）を実装する。
- 入力されたコマンドをディスパッチする共通の仕組み（コマンドレジストリ）を用意する。
- 実行したコマンドを `.usagi/history.json` に追記する。

## 完了条件

- home/open 画面でプロジェクトを選択するとワークスペース画面に遷移する。
- コマンド入力欄に文字を打つと候補が Tab 補完され、↑/↓ で履歴をたどれる。
- 未知のコマンドはエラーメッセージを履歴ペインに表示する。
- 後続コマンド issue がこの基盤に乗る形で追加できる拡張点（trait / enum）が用意されている。
