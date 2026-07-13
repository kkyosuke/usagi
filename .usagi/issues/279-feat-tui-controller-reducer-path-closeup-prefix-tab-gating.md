---
number: 279
title: feat(tui): controller reducer path に Closeup prefix / tab-gating を投影する
status: todo
priority: medium
labels: [tui, closeup, input, controller]
dependson: [278]
related: [269, 267]
created_at: 2026-07-13T02:58:46.925715+00:00
updated_at: 2026-07-13T02:58:46.925715+00:00
---

## 目的

#278 で live runtime を `LiveInputClassifier` prefix に統一した Closeup 操作 model を、forward-looking な controller reducer path（`AppState` / `render_home`、parity suite が駆動）へも投影し、両 path を完全に収束させる。

## 調査根拠

- controller path は `AppKey::CtrlO` / `CtrlA` と `Overlay::Closeup` を持つが、tab/pane 認識・`render_home` での action overlay 描画・`Ctrl-O n`/`Ctrl-O p` の tab 巡回を持たない。
- tab 有無の signal は `AppEvent::LivePaneAvailability(bool)`（`state.has_live_pane`）で既に controller に届く。tab 巡回は pane を所有する側へ effect（例: `Effect::SelectTab`）で委譲する。

## スコープ

- `render_home` に Closeup action overlay を描画し、`has_live_pane` の有無で表示を出し分ける（無→自動表示、有→`CtrlA` で表示）。
- `AppKey` に `CtrlN` / `CtrlP` を追加し、`classify_management_input` と reducer に mode-aware に配線。tab 巡回は pane runtime へ effect で渡す。
- Closeup に入るときに `has_live_pane` が false なら action overlay を自動で開く。
- parity scenario と純粋 reducer tests で両 path の遷移一致を固定する。

## 対象外

- #278 が扱う live runtime 側の実装。
- PTY passthrough 配線（別 issue）。

## 完了条件

- controller path で #278 と同じ 6 挙動が成立する。
- parity suite が両 path の Closeup 遷移一致を検証する。
- 実装・`document/03-tui.md`・regression tests が同じ PR に含まれ coverage 100% を満たす。
