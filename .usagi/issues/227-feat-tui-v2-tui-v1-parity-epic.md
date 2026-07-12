---
number: 227
title: feat(tui): v2 TUI v1-parity の残作業を追跡する Epic
status: done
priority: high
labels: [tui, epic, parity]
dependson: []
related: []
created_at: 2026-07-12T21:10:44.592323+00:00
updated_at: 2026-07-12T23:19:16.330873+00:00
---

## 目的

[parity 受け入れ契約](../../document/proposals/06-tui-v1-parity.md) を実装可能な issue DAG として追跡する。契約本文は proposal を正本とし、この Epic は work breakdown・依存・進捗だけを所有する。

## 子 issue

- 先行 pure/runtime: #228 renderer、#229 event pump、#230 entry fake、#231 lifecycle、#232 pane、#233 phase/feedback、#237 quit。
- daemon 結合: #234 lifecycle (D2)、#235 pane (D1/D3/D4/D6)、#236 phase/feedback (D2/D3/D5/D6)、#238 entry/runtime (D1)。
- B: #239 Open、#240 New、#241 Config、#242 Overview/top-level UX、#243 preview/diff/PR/text、#244 note/todos/decisions/env。
- release quality: #245 fake/golden、#246 real PTY、#247 data compatibility/docs fold-in。

## スコープ

- A の未達 acceptance と B の後回し surface、release quality を子 issue に分解する。
- fake/純粋 slice を daemon IPC 結合から分離し、結合 slice は対応する daemon issue を待つ。

## 対象外

- #222〜#226で完了した parity 設計、controller、input classifier、Home projection、modal registry dispatch の再実装。
- daemon/core の所有範囲や IPC wire 契約の変更。

## 完了条件

- 子 issue の dependson が実装順を表し、全 A acceptance と B/release quality の担当が追跡できる。
- 実装済みの挙動だけを仕様へ畳み込む。

## 検証

- `usagi issue graph` で DAG を確認する。
