---
number: 720
title: fix(daemon): 完了報告の再試行で dispatch status を収束させる
status: done
priority: high
labels: [v2, daemon, dispatch, correctness, retry]
dependson: []
related: [322, 323, 402]
created_at: 2026-08-24T11:30:54+00:00
updated_at: 2026-08-24T11:34:33.831703+00:00
---

## 問題

Agent の完了・失敗報告は caller inbox を先に永続化し、その後 run / agent status を遷移する。
inbox append 成功後に status 保存が失敗すると、再試行は既存 inbox を重複として即時 no-op にするため、
確定済み報告があるのに run / agent が `running` のまま永久に残る。

この不整合は session / Garden の終了表示、Agent の再利用可否、終了時の no-report 判定へ波及する。

## 修正方針

- exact run の確定済み inbox message を outcome の権威として扱う。
- 重複再試行でも確定済み kind から run / agent status を冪等に収束させる。
- 再試行 request の kind・summary・artifact で確定済み outcome を差し替えない。
- inbox だけが永続化された部分状態を構成する回帰テストを追加する。
- daemon / MCP の再試行整合性仕様を必要最小限更新する。

## 受入条件

- [ ] inbox に `Completed` が確定済みで run / agent が `running` の状態から、再試行で
      `Completed` / `Idle` へ収束する。
- [ ] inbox に `Failed` が確定済みなら、異なる再試行 request でも `Failed` / `Failed` へ収束する。
- [ ] inbox message は二重配送されず、最初の artifact が維持される。
- [ ] risk-based gate と PR CI の full test / coverage が green になる。
