---
number: 686
title: fix(tui): manual narrow Garden と wake-up input ownership を一致させる
status: todo
priority: medium
labels: [review, v2, tui, garden, input, bug]
dependson: []
related: [674]
parent: 671
created_at: 2026-08-16T22:32:02.034334+00:00
updated_at: 2026-08-16T23:34:22.036260+00:00
---

## Finding（P2 usability / foreground input ownership）

`origin/main` `c09dddcd61198124791e6707ac86d5b72d8dec8a` で #674 / #1492、後続 #1490 / #1494 / #1489 / #1495 の増分を再確認した。自動表示の narrow 抑止、Garden click の hitbox 解決、resize edge での close、overlay 中の PTY forwarding 抑止は実装された。#1494 / #1495はうさぎのprojection・pose・motion・選択表示を変更し、#1489はshared terminalのgeometry同期を変更したが、session plotのhitboxとGarden input routingは変更していないため、手動表示と一部の user activityが同じ foreground ownerに載っていない残件は継続する。

### 残っている不整合

1. **手動 `garden` は narrow terminal でも state を開く**
   - `submit_overview` は現在の geometry を見ずに `Overlay::Garden` を設定する。
   - renderer は 64×14 未満で通常 Home へ fallback する。
   - frame loop の `AppEvent::Resize` は size の edge でだけ Garden を閉じるため、既に同じ narrow size が state に入っている通常経路では close されない。
   - 結果として見た目は Home、state は Garden の invisible overlay が残り、次の入力が wake-up として失われる。

2. **wheel は idle timer を reset するが Garden を閉じない**
   - wheel は `Key::Live(ScrollUp|ScrollDown)` になる。
   - `intercept_live_terminal_control` が `wants_pane_control_input() == false` の covered pane control として reducer より前で消費する。
   - `Overlay::Garden` の wake-up reducer へ届かず、documented contract の「wheel を消費して Home へ戻る」を満たさない。

3. **一部の key vocabulary は安全に遮断されるが wake-up にならない**
   - `Ctrl-C` / `Ctrl-Q` は `update_overlay_control_chord` の fail-safe branchで消費され、Garden を閉じない。
   - `Ctrl-D`、raw `Passthrough`、非 Windows の `TerminalCopy` は `app_event_from_key` が `None` を返すため、Garden を閉じない。
   - `Key::Pointer(Drag|Up)` も pane-control interceptor で消費される。通常の left-button Down (`Key::Click`) は #1492 の `GardenClick` で正しく close / visit する。

live Closeup を背面に持つ場合も `wants_live_input()` は overlay により false になるため、上記入力が PTY bytes や quit effectへ漏れる問題は確認されなかった。残件は「最初の user activity を一度だけ Garden が所有して閉じる」という wake-up semantics の欠落である。

## #1492 で解消済みの範囲

- automatic Garden は `garden_fits` により 64×14 未満では開かない。
- `AppEvent::Resize` は geometry edge で Garden を閉じ、new size を保存する。
- left-button click は描画 frame と同じ hitbox から `Visit(SessionId)` / `Dismiss` に解決される。
- Garden overlay 中は ordinary character / paste を live PTYへ forwardせず、reducer が close する。
- covered pane control は背景 scroll / tab / pointer selection を変更しない。

## 修正方針

- Garden を PTY forwarding、Director picker、pane-control interceptor、Home reducerより前の exclusive foreground input owner にする。
- `Key::Click` だけは現在の frame hitboxで `GardenClick`へ解決し、それ以外の user activity（key / paste / control chord / terminal copy / raw passthrough / wheel / pointer gesture）は `Dismiss` として一度だけ消費する。
- resize は現在の geometry edge close を維持し、更新後の size を保存する。
- 手動 command の admission も renderer と同じ availability policy に載せる。最小サイズを重複定義せず、presentation が注入する availability または依存方向を守る共有 policy として一本化する。
- wake-up 後の残りの pointer gestureが背面terminal、sidebar、link-open、selectionへ作用しないよう、gesture単位の消費を固定する。

## 受入条件

- [ ] 63×14 / 64×13で手動 `garden` を実行しても Garden stateを残さず、次のkeyが通常Homeへ届く。
- [ ] 64×14以上では手動表示とautomatic表示の既存renderer / hitbox / click-to-Closeupを維持する。
- [ ] live terminalを背面に持つGardenで文字、paste、Ctrl-C、Ctrl-Q、Ctrl-D、terminal copy、raw passthroughを入力すると、PTY bytes / quit effectは0でGardenだけが閉じる。
- [ ] wheelはscroll offsetを変えずGardenだけを閉じる。
- [ ] click / drag / releaseは1 gestureとしてGardenが所有し、背面selection・sidebar・link-openを変更しない。うさぎのDownだけは既存どおり対応sessionをvisitする。
- [ ] resizeはGardenを閉じ、new sizeをstateへ保存する。
- [ ] productionと同じrouting順（foreground owner → PTY → pane controls → reducer）のmatrix testで全`Key` vocabularyを固定する。

## 根拠箇所

- `crates/tui/src/usecase/application/controller.rs::{submit_overview,update_overlay,update_overlay_control_chord}`
- `crates/tui/src/presentation/views/workspace.rs::{garden_fits,garden_click_at}`
- `crates/tui/src/presentation/mod.rs::{route_workspace_input_before_reducer,intercept_live_terminal_control,drive_workspace_controller}`
- `crates/tui/src/presentation/workspace_runtime.rs::{handle_key,wants_live_input,wants_pane_control_input}`
- `src/runtime/tui.rs::classify_terminal_input`
- `src/tui_input.rs::adapt_event`
