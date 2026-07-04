---
number: 105
title: feat(orchestration): 自律オーケストレーション運用モデル（root=オーケストレーション専任・変更は必ず session）
status: todo
priority: high
labels: [orchestration, epic]
dependson: []
related: [99, 100, 101, 104]
created_at: 2026-07-04T21:44:50.362065+00:00
updated_at: 2026-07-04T21:44:50.362065+00:00
---

## 目的（Epic）

usagi の自律オーケストレーションを、次の 3 原則が**技術的に担保された**運用モデルとして確立する。設計の正本は [document/proposals/01-root-orchestration.md](../../document/proposals/01-root-orchestration.md)。

1. **root（リポジトリルートで動くコーディネータ）はオーケストレーションのみ**を行う（issue の選択・順序付け、session の作成/委譲、進捗ポーリング、完了 session の除去、次タスク投入）。
2. **root は git 追跡下のリポジトリを一切変更しない**（issue の作成・更新、ドキュメント編集、コード編集、`main` へのコミット/PR をしない）。
3. **リポジトリに変更が入りうる作業（調査→issue 化・実装・修正・ドキュメント更新）は必ず session の worktree（ブランチ）で行い、PR で `main` に反映する**。

## 背景（現状のギャップ）

- issue ファイル（`.usagi/issues/*.md`）は git 追跡下なので、root が issue を作ること自体が repo 変更になる（原則 2 に反する）。→ 作業の**起源**を再設計する必要がある。
- `session_delegate_issue` は事前 issue 前提で、未コミットの issue は新 worktree の枝に乗らない（#104 で顕在化）。→ 起源フローとブートストラップを整える。
- 「status を書くのは当該 session だけ」の規約なのに、session がマージ後に除去されると誰も `done` にしない（#104 は #615 でマージ済みなのに `todo` のまま）。→ status ライフサイクルを設計する。
- 「root は repo を変更しない」が規約止まりで技術的な担保がない。→ ガードレールを入れる。

## 子 issue

このモデルを実装に落とす子 issue を本 Epic 配下にぶら下げる（各子の `## やること` / `## 受け入れ条件` 参照）。ガードレール（MCP 書き込み拒否・guard-workspace の root モード・pre-commit backstop）、起源フロー（ブリーフ委譲）、delegate のブートストラップ検証、status 単一書き手化、正本ドキュメント更新。

## 受け入れ条件（Epic 全体）

- root で `usagi mcp` を動かした状態で、issue/memory の書き込み系 tool・worktree 外の Edit/Write・`main` への commit がいずれも**技術的に拒否**される。
- 事前 issue なしのブリーフから triage session を起こし、その session が issue を起票して PR → `main` に反映できる。
- root は `main` にコミット済みの backlog を読んで ready な issue を委譲でき、委譲された session が着手時 `in-progress`・PR 前に `done` を**自枝で**立て、マージで `main` に `done` が乗る。
- 上記が [04-orchestration.md](../../document/04-orchestration.md) / [.agents/workflow.md](../../.agents/workflow.md) / [03-commands/03-mcp.md](../../document/03-commands/03-mcp.md) に反映され、proposal は正本へ畳まれる。
