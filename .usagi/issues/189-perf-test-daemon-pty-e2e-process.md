---
number: 189
title: perf(test): daemon/PTY E2E の process 起動と待機を短縮する
status: done
priority: medium
labels: [perf, test]
dependson: []
related: [181]
parent: 177
created_at: 2026-07-11T00:41:58.929117+00:00
updated_at: 2026-07-12T22:27:02.386684+00:00
---

## 背景

issue #181 の nextest duration 計測で daemon IPC E2E が 3.43〜7.68s の slow 上位を占めた。cleanup、signal、timeout、capture は nextest 反復でも正常だったが、daemon/PTY process lifecycle の待機が full suite を重くしている。

## 対応

- daemon 起動を test ごとに繰り返す必要性と shared fixture の安全性を検証する
- polling interval / timeout を correctness を落とさず短縮する
- 異常時 SIGKILL fallback と残留 process 検査を維持する

## 完了条件

daemon/PTY test の wall time を短縮し、cargo test/nextest の反復で failure と残留 process がない。
