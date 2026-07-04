---
number: 108
title: feat(hooks): pre-commit で workspace root（非セッション）チェックアウトのコミットを弾く backstop
status: done
priority: medium
labels: [orchestration, chore]
dependson: []
related: []
parent: 105
created_at: 2026-07-04T21:46:09.751222+00:00
updated_at: 2026-07-04T23:09:04.136271+00:00
---

## 背景

#106（MCP 書き込み拒否）・#107（guard-workspace root モード）は Agent 経由の repo 変更を止める最終防壁だが、フックが差し込まれない経路（人手のコミット、フック無効化、別ツール）での root コミットは素通しする。「変更は必ず session」を守るための安価な backstop として、pre-commit フックでもガードする。

既存の lefthook pre-commit は「ブランチ名チェック」を持ち、`.usagi/sessions/` 配下の worktree はブランチ名 `usagi/<name>` のため命名チェックを免除している（[06-conventions.md#git-hookslefthook](../../document/06-conventions.md#git-hookslefthook)）。この「session worktree かどうか」の判定を再利用できる。

## やること

- lefthook の pre-commit に、**コミットが workspace root のチェックアウト（`.usagi/sessions/` 配下でない）で行われた場合は拒否**するチェックを追加する。判定は既存のブランチ名免除と同じ「worktree パスが `.usagi/sessions/` 配下か」で行う。
- 誤検知を避けるため、対象はワークスペースルート（`usagi` を運用しているリポジトリ）に限る方針を決める（そもそも usagi をライブラリとして使う一般リポジトリの root コミットまで妨げない）。usagi 自身のリポジトリでの自律運用を守るのが目的。
- 緊急脱出（`LEFTHOOK=0` / `--no-verify`）は従来どおり残す（原則使わない）。

## 受け入れ条件

- workspace root のチェックアウトで `git commit` するとフックが拒否し、「変更は session 内で行う」旨を案内する。
- `.usagi/sessions/<name>/` 配下の worktree のコミットは従来どおり通る。
- ドキュメント（[06-conventions.md](../../document/06-conventions.md) の Git Hooks 節）に backstop を追記する。

## メモ

これは backstop であり、一次防壁は #106 / #107。ローカル hook は迂回可能なので、`main` 側のブランチ保護（GitHub branch protection、既存 [enforce-pr-base.yml](../../document/06-conventions.md#cigithub-actions)）と併せて多層で守る位置づけ。
