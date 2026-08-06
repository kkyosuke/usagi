---
number: 668
title: perf(tui): terminal click と drag selection を viewport snapshot から開始する
status: todo
priority: medium
labels: [review, v2, tui, terminal, uiux, performance, selection, pointer]
dependson: []
related: [307, 389, 637, 660]
parent: 664
created_at: 2026-08-06T20:39:26.700393+00:00
updated_at: 2026-08-06T20:39:26.700393+00:00
---

## Finding（P2 interaction latency / allocation）

live terminal pointer downは、dragになるかplain clickになるか未確定の時点で `WorkspaceUi::terminal_cells` → `TerminalSession::cells` → `VtScreen::cells_with_scrollback` を呼び、retained history全体を `Vec<String>` 化する。plain clickのpointer upではlink検出のため同じ全履歴snapshotをもう一度作る。

latest mainのrelease probeでは約10,024 retained rowsの `cells_with_scrollback` が中央値約5.4ms/回だった。通常のURL clickはdown/upで最低2回となり、frame projection・URL scan・browser effectを除いてもhistory長比例の同期costを払う。dragしない大半のclickに全10,000行copyは不要である。

## 修正方針

- pointer downは現在描画中の `TerminalViewProjection` と対応するANSI-free viewport cellsだけをsnapshotし、`row_offset` / terminal material revision / geometryとanchorを保持する。
- first Dragでselectionへ昇格する。viewport外へdragする場合だけ、必要方向のrow windowをincrementalに拡張するか、明示auto-scroll後の新windowを同じrevision/originで取得する。最初から全historyをcopyしない。
- plain clickのURL hit-testは既に描画用に計算したvisible logical-line/link materialを再利用し、up時に全historyを再snapshotしない。wrapped URLはviewport境界に接するlogical lineだけを拡張する。
- pointer gestureはTerminalRef、screen/material revision、row origin、geometryでfenceする。output/resync/focus changeでfenceが変わった場合、別cellのURLを開いたり別textをcopyせずgestureをsafe cancelする。
- final copy textはdrag開始時のimmutable snapshotに基づく既存契約を維持する。ANSI除去、CJK display columns、blank padding、multi-row selection parityを保つ。

## 受入条件

- 10,000行history上のplain clickとviewport内dragで、snapshot/rendered row数がhistory長ではなくviewport/logical-line長に比例する。
- pointer down/upの間にoutput、scroll、resize、focus switch、resyncが起きてもwrong link open / wrong copyがない。
- wrapped URL click、CJK、wide glyph、blank line/padding、reverse drag、copy shortcutの既存挙動を維持する。
- viewport外dragの拡張量とmemoryはhard boundを持ち、巨大selectionはtyped feedback/backpressureへ縮退する。

## 必須テスト・計測

- visited-row counterで10,000行historyのplain clickがviewport+wrapped neighborsだけを読むことをassertする。
- output/revision changeをdownとupの間へ挟み、gesture cancelを固定する。
- release benchmarkでplain click / small drag / large dragを100/1,000/10,000 rowsで比較する。

## 根拠箇所

- `crates/tui/src/presentation/mod.rs`: `handle_terminal_pointer`, `terminal_cells`
- `crates/tui/src/usecase/application/terminal_session.rs`: `cells`
- `crates/tui/src/usecase/application/terminal_selection.rs`: immutable viewport snapshot
- `crates/tui/src/usecase/application/terminal_screen.rs`: row-window projection / link scan
