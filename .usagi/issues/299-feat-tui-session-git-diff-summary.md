---
number: 299
title: "feat(tui): session 一覧に非ブロッキング Git 差分 summary を表示する"
status: done
priority: high
labels: [tui, git, session]
dependson: [288]
related: [12, 33]
created_at: 2026-07-15T00:00:00+00:00
updated_at: 2026-07-15T00:30:00+00:00
---

# session 一覧の Git 差分 summary

## 目的

v1 の sidebar と同様に、v2 の session 行で integration base との差分状態と変更行数を確認できるようにする。Git の subprocess が初回描画や入力を止めないことを必須とする。

## 完了条件

- `origin/HEAD` がある repository はその remote base を優先し、ない場合は local `main` を基準にする。
- session 行は ahead / behind commit 数と追加 / 削除行数を表示する。
- Git inspection は background worker で実行し、完了前・取得不能時・base branch 自身では summary を表示しない。
- core の Git query と TUI の render を unit test し、TUI 仕様ドキュメントを更新する。
