---
number: 753
title: refactor(daemon): session action の dispatch を分割して行数上限の余裕を作る
status: todo
priority: high
labels: [v2, daemon, architecture, maintainability]
dependson: []
related: []
created_at: 2026-09-13T00:00:00+00:00
updated_at: 2026-09-13T00:00:00+00:00
---

## 問題

`src/runtime/daemon/dispatch.rs` は 5981 行で、`tests/architecture.rs` の上限 6000 行まで**残り 19 行**である。
このファイルに触れる変更は、機能と無関係に architecture test で落ちる。

落ち方も悪い。architecture test が先に失敗するため `Rust full test` と `Rust coverage` の両方が赤くなり、
**coverage は計測されないまま終わる**。したがって「行数超過」と「本当のカバレッジ不足」を 1 回の CI では
切り分けられず、修正 → CI の往復が最低 2 回増える。

実際に #749 と #750 の実装でこの上限を 2 回超えた。

## 方針

- `dispatch_session_action` の巨大な match を、責務ごとのファイルへ分割する（例: session lifecycle /
  scratchpad（note・todo・decision）/ delegation / workflow / PR・agent 観測）。
- 分割は**移動に徹する**。判定順序、エラー種別、`#[coverage(off)]` の帰属、caller 検証の位置を変えない。
- 属性と関数の間に別の item を挿入しない（#749 / #750 で `record_session_lineage` の `coverage(off)` を
  奪う事故が起きた）。
- 分割後の各ファイルにも行数上限を設け、同じ壁に静かに戻らないようにする。

## 受入条件

- [ ] `dispatch.rs` が上限に対して十分な余裕（目安 20% 以上）を持つ。
- [ ] 分割後のファイルにも architecture test の行数上限がある。
- [ ] 既存テストがすべて緑で、`coverage(off)` の件数と帰属が変わらない。
- [ ] 判定順序・エラー種別・caller 検証が分割前と同じであることを、既存の E2E で確認する。
