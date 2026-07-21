---
number: 453
title: 全コードレビュー修正 backlog（origin/main bfd59ec6）
status: done
priority: high
labels: [review, epic]
dependson: []
related: [400]
created_at: 2026-07-20T12:05:18.222534+00:00
updated_at: 2026-07-21T14:39:27.215826+00:00
---

## 問題・影響

`origin/main` `bfd59ec6`（指定されたレビュー基準 `9c3160d9` を包含）の全コードレビューで、secret 継承、restart 時の二重 spawn、誤った成功 replay、出荷中 v1 の workspace 越境など、既存の完了済み issue だけでは閉じない finding が確認された。finding を一括修正にすると review・rollback・優先度判断ができないため、本 epic を対応表の正本とし、実装単位は子 issue に分離する。

`v1/` は現在の出荷物、root workspace は v2 である。両版を同じ子 issue に含めるのは、共有 `scripts/install.sh` が原因で同時修正が必要な #461 だけとする。未完了 backlog で受入条件を完全に覆う finding は #405 と #406 を再利用し、新規 issue を重複作成しない。

採番中に別 session の Draft PR #1142 が #410〜#456 を並行作成したため、本 epic は #453、子は #457 から始まる。同 Draft は基準が古く、今回必須の責務分割・本文区分を満たさないため main backlog とはみなさず、本 backlog PR を正本として supersede する。番号 gap は再利用しない。

## 成立条件 / 再現フロー

- v2 daemon/TUI/core は production composition を実際に通る再起動・実 PTY・IPC テストで確認する。
- v1 は `v1/` の CLI/TUI/MCP/orchestrator を出荷経路として評価し、root v2 の実装を根拠に「修正済み」とみなさない。
- 完了済み issue は現在の production code と履歴を再確認し、現行受入条件を満たさない回帰には新しい issue を作る。

## 対象責務と非対象

対象は #457〜#501 の子 issue、および既存 #405・#406 への対応付け。各子は 1 つの独立した不変条件を所有する。実装修正、既存 issue の status 変更、既存 #323・#390 の同番号ファイルの履歴改変は本 triage PR の非対象とする。

## 受入条件

- [ ] すべての review finding が既存または新規 issue に一意に対応する。
- [ ] 各子 issue が priority、version/area labels、parent、必要最小限の `dependson` / `related` を持つ。
- [ ] 各子 issue に問題・再現、責務境界、受入条件、必須回帰テスト、docs/移行影響が記載される。
- [ ] P0/High は priority `high`、cleanliness/Medium は priority `medium` とする。

## 必須回帰テスト

本 epic 自体はコードを変更しない。backlog PR で CLI read-back、番号・関係・重複監査、`git diff --check`、変更 issue Markdown の link check を実行する。Rust gate は各実装子 issue で行う。

## docs / 移行影響

本 PR は backlog のみを変更し、runtime・永続形式・利用者挙動を変更しない。各子 issue は影響する `document/`、`v1/README.md`、release artifact、snapshot/cache migration を個別に明記する。
