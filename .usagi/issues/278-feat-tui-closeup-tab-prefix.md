---
number: 278
title: feat(tui): Closeup 入力を tab 有無で管理/ライブ prefix に一元化する
status: done
priority: high
labels: [tui, closeup, input, pane]
dependson: []
related: [267, 269, 265]
created_at: 2026-07-13T02:58:31.457160+00:00
updated_at: 2026-07-13T03:40:01.180037+00:00
---

## 目的

live Workspace runtime（`presentation/mod.rs` + `workspace.rs`、合成ルート `src/runtime/tui.rs` が駆動）の Closeup 操作感を、v1 の live terminal `Ctrl-O` prefix 契約に揃える。Closeup の tab（`PaneState`）有無で入力の所有者を切り替え、`LiveInputClassifier`（prefix の SSoT）が live runtime を end-to-end で駆動するようにする。

要望（1〜6）:

1. Switch で行の Enter は Closeup に入る。
2. Closeup で prefix `Ctrl-O`（leader → `o`）は Switch に戻る。
3. Closeup に tab が無いときは Closeup action modal を表示する。
4. Closeup に tab が 1 つ以上あるときは tab 表示を前面にし、action modal は表示しない。
5. tab がある Closeup で prefix `Ctrl-O a` を押すと Closeup action modal を表示する。
6. Closeup の prefix `Ctrl-O n` / `Ctrl-O p` でそれぞれ次/前の tab に切り替える。

## 調査根拠

- `LiveInputClassifier`（`crates/tui/src/usecase/terminal_input.rs`）が `Ctrl-O` leader prefix を SSoT として所有し、`LiveTerminalAction::{Switch, OpenCloseupModal, NextTab, PreviousTab, Agent, CloseTab, QuitConfirmation}` を既にテスト付きで実装している。live terminal への passthrough もここが所有する（`Ctrl-O`・`Ctrl-^` 以外は passthrough）。
- 一方 live runtime は bare な `Key`（`usecase/application.rs`）で駆動され、合成ルート `read_key` が Ctrl 修飾を落とすため `Ctrl-O` prefix を表現できない。要望 1/3/4 は `presentation/mod.rs`（`step_switch` / `render_workspace` の `!has_panes()` gate）に既にあるが、2/5/6 が欠けている。
- #269 は controller reducer path（`AppState`）の management chord 契約を固定済み。本 issue は live runtime 側を classifier に寄せて統一する（両 path の完全収束は #NEXT_FOLLOWUP で追う）。

## スコープ

- `Key` に `Live(LiveTerminalAction)` seam を追加し、合成ルート `read_key` で `LiveInputClassifier` を保持して `Ctrl-O` leader を解決、`Key::Live(action)` として presentation へ渡す。非 prefix キーと `Ctrl-C=Quit` は従来の Key マッピングを保つ（passthrough を壊さない）。
- `presentation/mod.rs` の Closeup 入力を tab 有無で所有者分けする:
  - tab 無し → action modal を表示（既存）。Enter→Closeup で surface。
  - tab あり → action modal は隠し、tab focus。`Ctrl-O` leader の chord（Switch/OpenCloseupModal/Next/PreviousTab/CloseTab/Agent/Quit）で操作。`Ctrl-O a` で action modal を forced 表示。
- `render_workspace` の modal gate を `mode==Closeup && (!has_panes() || closeup_action_forced)` にする。
- v2 正本 `document/03-tui.md` に tab-gated input ownership・prefix table・modal 表示規則を記載する。
- table-driven regression tests（`presentation/mod.rs`、`FakeTerminal` が `Key::Live(..)` を発行）を追加し、coverage 100% を維持する。

## 対象外

- controller reducer path（`AppState` / `render_home`）への同 model 投影（→ follow-up issue）。
- `pane_runtime` 経由の PTY への実 passthrough 配線（tab ありでの非 prefix キーは現状 sink のまま。別 issue）。
- daemon / IPC / Closeup command registry の変更。

## 完了条件

- Switch の Enter で Closeup に入る。
- Closeup（tab 無し）で action modal が表示される。
- Closeup（tab あり）で action modal は隠れ、`Ctrl-O a` で表示、`Ctrl-O o` で Switch、`Ctrl-O n`/`Ctrl-O p` で tab 巡回する。
- 非 prefix キーと `Ctrl-C` の従来挙動が保たれる（passthrough を壊さない）。
- 入力・描画・`document/03-tui.md`・regression tests が同じ PR に含まれ、coverage 100% を満たす。
