---
number: 583
title: refactor(daemon,tui): agent phase 語彙とランキングを共有 SSoT へ統合する
status: in-progress
priority: high
labels: [daemon, tui, ssot, refactor]
dependson: []
related: []
created_at: 2026-07-30T10:46:43.943974+00:00
updated_at: 2026-07-30T22:30:11.927375+00:00
---

## 背景

`crates/core/src/domain/session_lifecycle.rs:29-78` の `AgentPhase` が `as_token()` / `parse_token()` を持つ closed vocabulary の正本である（`parse_token("interrupted")` は `None` を返すテストが `session_lifecycle.rs:701` にある＝`"interrupted"` は正本の語彙に含まれない）。

しかし `crates/daemon/src/usecase/agent_ipc.rs` は同じ語彙を独自に再実装している。

- `runtime_phase`（`agent_ipc.rs:2436-2444`）と `reported_phase`（`agent_ipc.rs:2455-2463`）が `&'static str` トークンを `AgentPhase::as_token()` を経由せず直接ハードコード（`"running"` / `"ready"` / `"exited"` / `"ended"` / `"waiting"`）し、さらに正本の closed vocabulary に存在しない `"interrupted"`（`agent_ipc.rs:2442`）と `"none"`（`agent_ipc.rs:526`）を独自に追加している。
- この文字列は `session_phase()`（`agent_ipc.rs:518-527`）の戻り値としてそのまま `src/runtime/daemon.rs:4164` で IPC wire の `agent_phase` フィールドに載る。
- TUI 側の消費者 `src/runtime/tui.rs:2751-2765`（`provider_resume_projection`）は `phase == "interrupted"` を生文字列で比較しており、daemon 側の文字列と手動で同期する契約になっている（共有定数もコンパイル時の保証もない）。
- 加えて `crates/tui/src/usecase/application/controller.rs:634-663` の `TargetPhase::rank()`（`Absent(0) < Ready(1) < Running(2) < Waiting(3) < Done(4)`）は、daemon 側 `reported_phase` のドキュメントコメント（`agent_ipc.rs:2453-2454`: "Their relative order mirrors the Home aggregation (`done > waiting > running > ready`)"）が明言する通り、daemon の集約順を **手動で複製した別実装** である。

いずれも「同じ判定規則を2箇所で個別に保守する」形になっており、どちらか一方だけを変更すると気づかれずに drift する。特に `"interrupted"` はテストで明示的に非語彙とされているにもかかわらず wire 値として本番運用されている。

## 対象

- `runtime_phase` / `reported_phase` のトークン文字列を `AgentPhase::as_token()` 経由（または `AgentPhase` 自体を返す）に変更し、独自ハードコード文字列をやめる。
- `"interrupted"` を `AgentPhase` の closed vocabulary に正式に追加するか、既存 phase（例 `Exited` 系）へ統合するかを設計判断し、`session_lifecycle.rs` の test（`agent_phase_tokens_and_wired_hook_events_stay_a_closed_vocabulary`, `session_lifecycle.rs:685` 付近）と整合させる。TUI 側の生文字列比較（`src/runtime/tui.rs:2751-2765`）も同じ enum/定数を参照する形にする。
- daemon の phase 集約順位と TUI `TargetPhase::rank` の集約順位を、どちらかをもう一方が呼ぶ形、または `usagi-core::usecase` に共有ロジックとして1つだけ持つ形に統合する（両面が使う判定ロジックは `usagi-core/usecase` に置くという `document/02-architecture.md` の方針に合わせる）。

## 受入条件

- [ ] daemon が生成する agent phase の wire 文字列は、`AgentPhase::as_token()`（または同等の単一関数）以外の場所でハードコードされていない。
- [ ] `"interrupted"` を含む全ての wire phase 値が、`AgentPhase` closed vocabulary の一部として parse 可能（またはそのように設計変更されている）。
- [ ] TUI 側の phase 文字列比較が、daemon 側と共有する型/定数を経由し、生文字列リテラルの手動同期に依存しない。
- [ ] daemon の phase 集約順位と TUI `TargetPhase::rank` が同じ実装（共有関数）を参照する、または少なくとも1つのテストで両者の順序が一致することを検証する。
- [ ] 既存の `session_phase` / `TargetPhase::phase_for` の外部挙動（IPC wire 形式・TUI 表示）に regression がない。
