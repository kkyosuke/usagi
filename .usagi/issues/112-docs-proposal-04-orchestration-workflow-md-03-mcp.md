---
number: 112
title: docs: 自律オーケストレーション運用モデルを正本へ反映し proposal を畳む（04-orchestration / workflow.md / 03-mcp）
status: done
priority: medium
labels: [orchestration, docs]
dependson: [106, 107, 109, 110, 111]
related: []
parent: 105
created_at: 2026-07-04T21:47:43.794426+00:00
updated_at: 2026-07-08T22:37:13.241364+00:00
---

## 背景

設計 proposal（[document/proposals/01-root-orchestration.md](../../document/proposals/01-root-orchestration.md)）は「未実装を spec に書かない」規約（[06-conventions.md#記載実装済み](../../document/06-conventions.md#記載実装済み)）を守るため proposals 配下に分離してある。#106–#111 で機構が実装され挙動が確定したら、その内容を**正本ドキュメントへ畳み込み**、proposal は撤去（またはリンクだけ残す）する。各機構 issue は自分の局所ドキュメントを同 PR で更新するが、本 issue は**運用モデルの横断ナラティブ**と**既存記述の改訂**を担う。

## やること

- [04-orchestration.md](../../document/04-orchestration.md) に「自律オーケストレーション運用モデル（root と session の責務分界／起源フロー／status ライフサイクル／ガードレール）」を正本として追記する。
- [.agents/workflow.md](../../.agents/workflow.md) の記述を改訂する。現行は「`main` で root が行うのは issue の**定義**（作成・本文編集）のコミットと delegate だけ」とあるが、新モデルでは **root は issue の定義もしない**（起源はトリアージ session）。この差分を反映し、status 単一書き手の記述と整合させる。
- [03-commands/03-mcp.md](../../document/03-commands/03-mcp.md) に、root での書き込み系 tool 拒否（#106）・`session_delegate_brief`（#109）・`session_delegate_issue` の基点検証（#110）・`issue_to_prompt` の status 指示（#111）を反映する（各機構 PR で部分更新済みなら整合確認）。
- proposal を正本へのリンクに置き換えるか、履歴として残すかを決める。[document/README.md](../../document/README.md) の目次と [document/proposals/README.md](../../document/proposals/README.md) を整合させる。
- markdown-link-check（lychee）を通す（アンカー・相対リンクの整合）。

## 受け入れ条件

- 04-orchestration / workflow.md / 03-mcp が新モデルと一致し、root=issue 定義可の旧記述が消える。
- 「記載＝実装済み」を満たす（未実装表現なし）。proposal は正本へ畳まれ、目次が整合する。
- リンクチェック CI が通る。
