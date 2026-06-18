---
number: 4
title: セッション切り替え（session switch）
status: done
priority: high
labels: [tui]
dependson: [2, 3]
related: []
created_at: 2026-06-16T22:59:02.160254+00:00
updated_at: 2026-06-16T23:08:03.542560+00:00
---

# セッション切り替え（`session switch`）

## 概要

作成済みのセッション（worktree）間を切り替える TUI 内コマンドを実装します。現在アクティブなセッションを切り替えることで、後続のコマンド（`ai` / `terminal` / `diff` など）の実行対象が切り替わります。

> 当初は独立した `space` コマンドとして設計していましたが、セッション操作は `session` に集約する方針に変更し、`session switch` サブコマンドとして実装しました。

## やること

- `session switch <name>` または一覧からの選択（worktree 一覧の Enter）で、アクティブな worktree を切り替える。
- `session switch`（引数なし）でセッション一覧を表示し、アクティブなものを強調する。
- 現在アクティブな worktree をワークスペース画面上で視覚的に強調表示する。
- アクティブな worktree のパスを以降のコマンド実行のカレントディレクトリとして扱う。

## 完了条件

- 複数セッションがあるとき `session switch` で対象を切り替えられ、アクティブ表示が更新される。
- 切り替え後に実行する `terminal` / `ai` などが正しい worktree 配下で動作する。
