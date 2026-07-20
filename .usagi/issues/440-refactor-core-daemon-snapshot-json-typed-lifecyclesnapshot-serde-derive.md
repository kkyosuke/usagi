---
number: 440
title: refactor(core): daemon snapshot の JSON 手掘りを typed LifecycleSnapshot（serde derive）に置き換える
status: todo
priority: medium
labels: [refactor, core, review]
dependson: []
related: []
created_at: 2026-07-20T12:02:25.920413+00:00
updated_at: 2026-07-20T12:02:25.920413+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

daemon の lifecycle snapshot JSON を文字列キーで手掘りする箇所が 3 つある:

- `src/runtime/daemon.rs:369-383` — `available_worktree` が `"sessions"/"session_id"/"lifecycle"/"worktree_id"` を辿る。
- `src/runtime/daemon.rs:1394-1404` — `session_id_by_name` が `"sessions"/"name"/"lifecycle"/"session_id"` を辿る。
- `src/runtime/tui.rs:725-773` — `lifecycle_snapshot` が `"revision"/"workspace_id"/"root_worktree_id"/"sessions"` を手で読む。

付随して `src/runtime/tui.rs:704-708` と :715-719 に、`record.root` へ**同一値を二重代入**する dead code がある。

## 問題

スキーマが 3 実装に分散し、field 名の変更・追加で黙って壊れる（typo はコンパイルで捕まらない）。二重代入は読み手を混乱させる。

## 改善案（要検討）

- usagi-core に serde derive の typed `LifecycleSnapshot`（sessions・lifecycle の構造体）を置き、daemon 側の応答整形と client 側の読み取りの双方で使う。
- tui.rs:704-719 の root 二重代入を同時に解消する。
- 関連: dispatch_* 群の daemon crate 移設 issue #432（移設後の型共有が自然になる）。

## 受け入れ条件

- [ ] snapshot の JSON 手掘りが typed 構造体の (de)serialize に置き換わっている。
- [ ] root 二重代入が解消されている。
- [ ] wire 互換（既存 JSON 形）が保たれることがテストで固定されている。coverage 100% を維持。
