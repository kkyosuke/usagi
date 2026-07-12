---
number: 223
title: feat(tui): unified application controller と fake backend 基盤を追加する
status: done
priority: high
labels: [tui, rust]
dependson: []
related: [222]
created_at: 2026-07-12T12:53:26.111794+00:00
updated_at: 2026-07-12T13:00:23.751386+00:00
---

## 目的

v2 TUI の Home state / event / effect を daemon 実装なしで検証できる純粋な application controller と fake backend seam を導入する。

## スコープ

- AppState / AppEvent / Effect と reducer を usagi-tui usecase 層に追加する。
- Home の Switch / Closeup、overlay origin、selected / active target をモデル化する。
- TUI-local BackendPort と FakeBackend event queue を導入し、table-driven scenario test で A-MODE-1 / A-HOME-1 の開始点を固定する。
- 既存 Workspace view への最小 adapter を追加し、既存表示・modal・presentation::run を後退させない。

## 対象外

- daemon wire / IPC、実 daemon adapter、crossterm input mapping、src/main.rs、dummy command registry の置換。

## 完了条件

- reducer と fake seam がコンパイル・テスト済みである。
- Home は Switch / Closeup のみ、overlay は origin に戻り、selected と active を分離する。
- issue を同一 PR で done にする。
