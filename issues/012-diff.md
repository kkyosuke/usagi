---
number: 012
feature: diff
title: TUI Diff ビューア
status: todo
priority: medium
category: tui
dependson: [002, 003]
ref: usagi.ai issue/diff.md
---

# TUI Diff ビューア

## 概要

現在のセッションと main ブランチの差分を TUI 上で確認できるビューアを実装します。AI エージェントが何を変更したかを、TUI から離れずに素早くチェックできるようにします。

## やること

- `diff` コマンドでアクティブセッションと main の差分を取得する。
- 差分を TUI 上で見やすく表示する（ファイル単位の一覧 + ハンク表示、スクロール対応）。
- 追加/削除行の色分け表示を行う。

## 完了条件

- `diff` でアクティブセッションの変更内容が TUI 上に表示される。
- ファイルを選択して差分の詳細をスクロール閲覧できる。
