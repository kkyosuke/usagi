---
number: 221
title: docs(daemon): PTY broker／FD handoff による crash 継続を将来設計する
status: todo
priority: low
labels: [design, daemon, terminal]
dependson: [209]
related: [168, 213]
created_at: 2026-07-12T11:40:16.705437+00:00
updated_at: 2026-07-12T12:06:04.439149+00:00
---

## 目的

v2 MVP が明示する「daemon crash後はPTY master fdを復元できず、画面・入出力へ再attachできない」という境界を越える場合の将来案を、MVP実装と分離して評価する。

## 調査候補

- daemon外の小さなPTY brokerがmaster fdとvt100/output journalを所有する構成。
- planned restart時のUnix `SCM_RIGHTS` によるFD handoff。
- broker自体のcrash、認証、protocol generation、upgrade、process supervision、Windows portability。
- old daemonをdrainingさせる現行rolloverとの複雑性・memory/FDコスト・故障領域比較。

## 成果物

- failure matrixとthreat model。
- broker方式／handoff方式／現状のexplicit orphan契約の比較表。
- 採否、採用時のprotocol／ownership変更、独立した実装issue分割。

## 非目標

本issueはMVPの依存ではなく、PIDだけからPTYをadoptできるものとして扱わない。
