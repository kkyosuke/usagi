---
number: 31
title: ルートモード（どのセッションにも属さない作業）
status: done
priority: medium
labels: [tui]
dependson: [6, 26]
related: []
created_at: 2026-06-16T23:06:10.928328+00:00
updated_at: 2026-06-16T23:08:33.339589+00:00
---

# ルートモード（どのセッションにも属さない作業）

## 概要

ホーム画面の worktree 一覧の先頭に、どのセッションにも属さない **ルート行（`root`）** を常設します。ルート行を選んで `terminal` / `agent` を実行すると、セッションの worktree ではなく**ワークスペースルート**で対話型シェル / Agent CLI が起動します。

従来は「worktree を未選択のとき（一覧が空のとき）だけ暗黙的にワークスペースルートへフォールバックする」挙動でしたが、これを明示的な選択肢にしました。ワークスペースを開いた直後はこのルート行が選択・アクティブ状態（＝どのセッションにも入っていない状態）になります。

## やること

- worktree 一覧の先頭（index 0）に、セッションに属さないルート行を常設する。
- ルート行を選択（カーソル）した状態で `terminal` / `agent` を実行したら、ワークスペースルートでシェル / Agent を起動する。
- ワークスペースを開いた直後の既定はルート行（カーソル・アクティブともに）にする。
- `session switch root` でもルート行をアクティブにできるようにする。
- ルート行をサイドバーで視覚的に区別する（`⌂` アイコン・`root` ラベル・status はプレースホルダ）。

## 完了条件

- ルート行を選んで `terminal` / `agent` を実行すると、worktree ではなくワークスペースルートで起動する。
- worktree 行を選べば従来どおりその worktree で起動する。
- `↑`/`↓` でルート行と worktree 行を行き来でき、末尾↔ルート行でラップする。
- `session switch root` と worktree 一覧の Enter（ルート行上）でルートへ切り替えられる。

## 実装状況

`terminal`（[006](006-terminal.md)）/ `agent`（[026](026-agent.md)）の経路をそのまま活かして実装。event loop のディレクトリ解決（選択中 worktree が無ければワークスペースルート）に合わせ、ルート行を「worktree を持たない行」として表現した。

- `presentation/tui/home/state.rs`：`WorktreeList` の選択可能行を「ルート行（index 0）＋ worktree（index 1..=N）」に拡張。`selected()` / `active()` はルート行で `None` を返し（＝ワークスペースルート）、`root_selected()` / `root_active()` を追加。`activate_selected` / `activate_by_name`（`root` 名対応）/ `refs`（ルート行を先頭に）/ カーソル移動のラップを更新。`ROOT_NAME` 定数を公開。
- `presentation/tui/home/ui.rs`：左ペインに常設のルート行（`root_row`：`⌂` アイコン・`root` ラベル・`—` ステータス）を描画。worktree 行は index +1 でマーカーを判定。
- `presentation/tui/home/event.rs`：`Effect::OpenTerminal` / `OpenAgent` の解決は既存のまま（`selected()` が `None`＝ルート行ならワークスペースルート）。ドキュメントコメントを更新。
- `presentation/tui/home/command.rs`：`terminal` / `agent` の説明をルート対応に更新。
