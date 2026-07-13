---
number: 277
title: feat(tui): run Home pane lifecycle through the interactive workspace runtime
status: todo
priority: high
labels: [tui, runtime, daemon]
dependson: [276]
related: [263, 265, 271, 274]
created_at: 2026-07-13T02:33:33.242088+00:00
updated_at: 2026-07-13T02:33:33.242088+00:00
---

## 目的

`src/runtime/tui.rs` の interactive `launch_workspace` が旧 `WorkspaceView` loop を直接起動している状態を解消し、Home controller、`PaneRuntime`、`AgentLaunchAdapter` を実際の `cargo run` 経路で合成する。

## 背景

#276 は v1 parity の空 pane と pending chip の純粋 view/widget を実装する。現行 interactive runtime はこれらの `HomeProjection` をまだ描画しないため、daemon accepted → pending → fenced live/failure と tick animation を実端末へ投影する合成 adapter が必要である。

## 完了条件

- interactive workspace loop が `RuntimeEvent::Tick` を Home controller に渡して再描画する。
- Closeup agent effect を `AgentLaunchAdapter` に dispatch し、accepted 中の pending、success の live tab、failure feedback を同じ `PaneRuntime` から描画する。
- terminal transport/stream/resize/detach を既存 `PaneRuntime` port で接続し、legacy `WorkspaceView` の tab state を source of truth にしない。
- fake terminal + fake daemon を使う integration test と、runtime adapter の回帰 test を追加する。
- `document/03-tui.md` の実行経路記述を実装済み状態に更新する。
