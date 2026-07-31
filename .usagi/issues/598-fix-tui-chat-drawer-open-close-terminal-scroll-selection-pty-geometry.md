---
number: 598
title: fix(tui): chat drawer の open/close が terminal scroll / selection と PTY geometry を壊さないようにする
status: in-progress
priority: medium
labels: [tui, bug, terminal, agent]
dependson: [596]
related: [577, 597]
created_at: 2026-07-31T11:06:02.375573+00:00
updated_at: 2026-07-31T12:09:10.667829+00:00
---

## 症状

chat drawer（指示モード）を開閉すると、その前後の terminal 表示状態が失われる。

- drawer を閉じて開き直すと、drawer 側 conversation の scroll 位置・選択範囲が失われる。
- 逆に drawer を一度開くだけで、背後の managed pane の scroll 位置・選択範囲も失われる。
- 開閉のたびに PTY が drawer viewport ↔ 右ペイン viewport の間で resize され、full-screen TUI（codex など）が
  そのたびに再描画する。checkpoint の geometry fence 拒否 → resync 経路も開閉ごとに踏みうる。

`document/03-tui.md` は drawer の開閉について「開閉は Home mode、selected cursor、active managed session、
managed pane の selected tab、**terminal scroll / text selection を変更しない**」と規定しており、実装がこれに
違反している。

## 原因

1. **scroll / selection の破棄**: `LiveTerminalControls::sync_focus` は focus した terminal identity が変わるたびに
   `scroll` / `selection` / `feedback` を reset する。drawer の open/close は
   `WorkspaceRuntime::follow_active_target` 経由で active target を root ↔ session と切り替えるため、
   `focused_terminal()` が変わり、開閉ごとに両側の状態が reset される。controls は「現在 focus している 1 本」しか
   持たないため、terminal ごとの view 状態が構造的に保持できない。
2. **detach / 再 attach と geometry 往復**: `foreground_terminal_geometry` が drawer open で drawer viewport、
   close で右ペイン viewport を返し、`sync_foreground_terminal` が非 focus 側を毎回 detach するため、開閉ごとに
   「detach → 新規 attach → resize → checkpoint restore」が走る。

## 変更方針

- **terminal ごとの view 状態（scroll / selection / feedback）を terminal identity で保持する**。
  `LiveTerminalControls` を「focus 中の 1 本」から「terminal ごとの状態を bounded に保持し、focus はその参照」に
  変える。focus が戻ったら以前の scroll / selection を復元する。保持数は bounded にし、terminal が閉じたら破棄する。
- **開閉での PTY resize 往復を減らす**。少なくとも次を満たす。
  - drawer close 中も root conversation の geometry を無用に変えない（detach 中の terminal を右ペイン geometry へ
    resize しない）。
  - 同じ geometry へ戻る再 attach で checkpoint の geometry fence 拒否 → resync を招かない。
  - #596 で detached session を保持する方針を採るなら、開閉は subscription の release / 再取得だけで済ませ、
    screen の再構築を伴わないようにする。
- 「開閉が terminal scroll / text selection を変更しない」を、doc の文言どおりテストで固定する。

## 対象ファイル

- `crates/tui/src/presentation/live_terminal.rs`（`LiveTerminalControls` の保持単位）
- `crates/tui/src/presentation/mod.rs`（`controller_terminal_view` / `foreground_terminal_geometry` /
  `sync_foreground_terminal` / frame loop の呼び出し順）
- `crates/tui/src/presentation/workspace_runtime.rs`（`follow_active_target` の focus 遷移）
- `document/03-tui.md`（開閉の不変条件を実装に合わせて明記）

## 受け入れ条件

- drawer を開いて scroll / 選択した状態で閉じ、再度開くと scroll 位置と選択範囲が復元される。
- 背後の managed pane で scroll / 選択した状態で drawer を開閉しても、その scroll / 選択が保持される。
- 開閉 1 往復で PTY へ送られる resize が、必要最小限（geometry が実際に変わる分だけ）である。
- 同一 geometry へ戻る再 attach が checkpoint geometry 拒否 → resync を発生させない。

## テスト方針

- `cargo test -p usagi-tui presentation::live_terminal`（terminal 単位の状態保持 unit test）
- `cargo test -p usagi-tui presentation`（drawer 開閉往復で scroll / selection が保持される shell seam test、
  および resize 回数の観測テスト）
- `cargo test -p usagi --bin usagi`（frame loop 側の geometry 選択に回帰がないこと）

## 非目標

- drawer の描画 component 共有（#597）。
- input ledger の連続性（#596）。
- 保持数の上限を無制限にすること（bounded を維持する）。
