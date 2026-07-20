---
number: 450
title: refactor(tui): ANSI エスケープ走査の多重実装（widget/frame/modal/mascot ほか計 15 箇所、OSC 扱いに差）を widgets::ansi の単一 iterator に統合する
status: todo
priority: medium
labels: [refactor, tui, review]
dependson: []
related: []
created_at: 2026-07-20T12:04:46.842896+00:00
updated_at: 2026-07-20T12:04:46.842896+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

ESC シーケンス走査の独立実装が主要 7 実装＋インライン走査で計 **15 箇所**確認された:

- `crates/tui/src/presentation/widgets/mod.rs` — `display_width`（:173）・`clip_to_width`（:196）・`dim_ansi`（:290、OSC 対応あり: テスト :589-590）・`strip_ansi`（:381）。
- `presentation/frame.rs:371` — `ansi_sequence`。
- `widgets/modal.rs:524` — `columns`。
- `widgets/mascot.rs:162` — `strip_ansi`（別実装）。
- インライン走査: presentation/mod.rs:3281、workspace_runtime.rs:1501、layouts/mascot_screen.rs:96、views/open.rs:519、views/new.rs:710、views/workspace.rs:1747、views/splash.rs:74、views/welcome.rs:451、widgets/session_tab.rs:161。
- テストヘルパの strip コピー: `crates/tui/tests/parity_suite.rs:94`。

**挙動差が既にある**: OSC（`ESC]...BEL`）の扱いが実装ごとに異なる（dim_ansi は対応、他は CSI のみ等）。

## 問題

OSC を含む出力（タイトル設定等）で、幅計算・クリップ・strip の結果が経路によって食い違い、表示崩れの原因特定が困難になる。走査仕様の変更が 15 箇所の同期修正を要求する。

## 改善案（要検討）

- `widgets::ansi` に単一のトークン iterator（text / CSI / OSC / その他 ESC）を置き、display_width / clip / strip / dim をその上の薄い消費者として再実装する。
- インライン走査とテストヘルパも同 iterator（と共通 `strip_ansi`）へ置換する。

## 受け入れ条件

- [ ] ESC 走査の実装が 1 箇所になり、OSC の扱いが全経路で一貫する。
- [ ] 既存の描画（幅・クリップ・dim）が回帰しない（既存テスト維持＋OSC ケース追加）。
- [ ] coverage 100% を維持する。
