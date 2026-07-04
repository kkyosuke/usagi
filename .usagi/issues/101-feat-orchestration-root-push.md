---
number: 101
title: feat(orchestration): root エージェントへの push 型完了報告 — 子セッションの完了をコーディネータへ通知する
status: done
priority: high
labels: [orchestration]
dependson: []
related: [100]
created_at: 2026-07-04T05:09:58.100705+00:00
updated_at: 2026-07-04T21:40:56.020735+00:00
---

## 背景

子セッションのエージェントが完了（`ended`）してもデスクトップ通知と TUI バッジで**人間**に知らされるだけで、root 行で動くコーディネータの**エージェント**には届かない。#100 の `session_status` ポーリングでも完了検知はできるが、push 型があればポーリング間隔に依存せず即座に次のタスクへ進める。

## やること（案）

- 案 A: `session_prompt` の宛先に root 行を許す（例: `name: ":root"`）。root の live agent ペインへライブキュー配送する。子エージェントが完了報告プロンプトを自分で送る。
- 案 B: agent phase が `ended` へ遷移したとき、usagi のフック（`usagi agent-phase`）が root 宛の報告キューに「セッション <name> が完了」を積み、TUI の監視スレッドが root の live agent ペインへ配送する。
- どちらを採るか（または両方か）は #100 の実装後に、実運用のループで足りない点を見てから決める。

## 受け入れ条件

- 子セッションの完了が、人手なしで root のコーディネータエージェントの入力として届く。
- ドキュメント（03-mcp / 04-orchestration）を更新する。
