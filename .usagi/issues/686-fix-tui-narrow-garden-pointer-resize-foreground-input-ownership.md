---
number: 686
title: fix(tui): narrow Garden と pointer/resize の foreground input ownership を一致させる
status: todo
priority: medium
labels: [review, v2, tui, garden, input, bug]
dependson: []
related: [674]
parent: 671
created_at: 2026-08-16T22:32:02.034334+00:00
updated_at: 2026-08-16T22:32:02.034334+00:00
---

## Finding（P2 usability / input ownership）

最新 `origin/main` の手動 `garden` command は controller で無条件に `Overlay::Garden` を設定し、presentation だけが 64×14 未満で Garden renderer を `None` にして通常 Home を描く。

このため狭い端末では見た目は Home のままなのに state は Garden であり、次の通常 key が invisible overlay の wake-up として握り潰される。さらに Garden の pointer / wheel / resize は現在の input routing と一致しない。

- `AppEvent::Pointer` は overlay があると `update_pointer` が inert にするため Garden を閉じない。
- wheel は `LiveTerminalAction::Scroll*` になり、reducer より前の pane-control interceptor に消費される。
- resize は size を更新するだけで Garden を閉じず、`Key::Resize` も reducerへは `Tick` として届く。resize後に最小サイズ未満になると invisible Garden state が残る。

`document/03-tui.md` は「最初の入力を消費して戻る」「64×14未満ではGardenを開かずHomeを保つ」と記載しており、現行state/input ownershipと不整合である。idle自動表示やrabbit click-to-Closeup自体は親 #674 の未完了scopeであり、本issueは現在公開済みの手動Gardenのforeground ownershipだけを扱う。

## 修正方針

- Garden availability（最小幅/高さ）をusecase側の一つのpolicyへ置き、controllerとrendererが同じ値を参照する。presentation定数をcontrollerへ逆依存させない。
- 最小サイズ未満の`garden` commandはinvisible `Overlay::Garden`を残さない。Home維持またはsafe noticeのどちらかを仕様とtestで固定する。
- GardenをPTY forwarding / pane controls / sidebar reducerより前のexclusive foreground input ownerにする。
- key / paste / click / wheel / resizeは、親 #674 がrabbit clickを接続するまではすべてGardenを閉じるwake-upとして一度だけ消費する。背面terminal、scroll、selection、sidebar、quitへ転送しない。
- resizeでGardenを閉じ、更新後のgeometryを保持する。

## 受入条件

- [ ] 63×14 / 64×13で`garden`を実行してもGarden stateを残さず、次のkeyが通常Homeへ届く。
- [ ] live terminalを背面に持つGardenで文字/paste/Ctrl-C/Ctrl-Qを入力してもPTY bytesとquit effectは0、Gardenだけが閉じる。
- [ ] click / drag / release / wheelはpane/sidebarを変更せずGardenだけを閉じる。
- [ ] resizeはGardenを閉じ、new sizeをstateへ保存する。
- [ ] 64×14以上の手動Garden表示とrenderer/hitboxの既存testを維持する。

## 根拠箇所

- `crates/tui/src/usecase/application/controller.rs::{submit_overview,update_overlay,update_pointer}`
- `crates/tui/src/presentation/views/workspace.rs::render_home_at`
- `crates/tui/src/presentation/mod.rs::{route_workspace_input_before_reducer,intercept_live_terminal_control}`
- `crates/tui/src/presentation/workspace_runtime.rs::wants_live_input`
