---
number: 125
title: refactor(tui): AgentState のフェーズ・グリフ/語/色の三重定義を単一テーブル化する
status: todo
priority: medium
labels: [refactor, tui, review]
dependson: []
related: [123]
created_at: 2026-07-04T23:15:59.844048+00:00
updated_at: 2026-07-04T23:15:59.844048+00:00
---

## 背景（なぜ問題か）

`panes.rs` の `AgentState` は `detail` / `icon_label` / `rail_icon` の 3 メソッドがそれぞれ 5 バリアント（Absent/Ready/Running/Waiting/Done）を `match` し、同じグリフ（`☾`/`▶`/`◆`/`✓`）と同じ色（`dim`/`success`/`warning`/`accent` ＋ `bold`）を独立に埋め込んでいる。フェーズ 1 つの色や記号を変えるには 3 メソッドを揃えて直す必要があり、SSoT に反する。

## 対象箇所

`src/presentation/tui/home/ui/panes.rs` の `impl AgentState` の `detail` / `icon_label` / `rail_icon`（および語 `ready`/`running`/`waiting`/`done`）。

## やること

- 各バリアントの `(phase_glyph: &str, word: &str, style: Style)` を返す 1 メソッド（例 `fn face(self) -> Option<(&str, &str, Style)>`、`Absent` は `None`）を用意する。
- `detail` / `icon_label` / `rail_icon` はそこから `AGENT_ICON + glyph (+ word)` を組み立てるだけにする。

## 受け入れ条件

- グリフ・色・語の定義がバリアントあたり 1 箇所になる。
- `detail`/`icon_label`/`rail_icon` の出力が現状と一致（既存の rows テストで担保）。カバレッジ 100% 維持。

## 補足

#123（panes.rs 分割）と同一ファイル。分割の前後どちらでも実施可能なため related。
