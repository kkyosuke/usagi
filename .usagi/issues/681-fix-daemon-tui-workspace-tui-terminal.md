---
number: 681
title: fix(daemon,tui): 同じ workspace を複数 TUI で開いても terminal 表示が壊れないようにする
status: done
priority: high
labels: []
dependson: []
related: []
created_at: 2026-08-13T23:45:53.617337+00:00
updated_at: 2026-08-14T00:33:16.663946+00:00
---

## 症状

同じ workspace を 2 つ以上の TUI（別々の端末ウィンドウ）で開くと、terminal pane の表示が崩れる。
端末ウィンドウのサイズが異なるときに顕著。

## 原因

daemon の terminal registry は terminal 1 件につき PTY を 1 つ所有し、その geometry を
**最後に `Resize` を送った client が上書きする**（last-writer-wins）。一方 client
（`TerminalSession`）は自分が同期した geometry で local screen を decode し続ける。

その結果、pane 幅の異なる 2 つの TUI が同じ terminal に attach すると

- PTY は後から resize した client の幅になる
- 先に attach していた client は自分の幅のまま decode するので、行の折返し位置・
  再描画位置がずれて画面が壊れる
- client 側の geometry fence（snapshot の geometry が pane と違えば snapshot を破棄）が
  さらに resync loop を招く

## 方針

PTY の geometry を **attach している client の viewport の各次元の最小値**にする
（tmux の既定 `window-size smallest` と同じ）。最小値なので、どの client も自分の pane より
大きい screen を渡されることがなく、余った領域は空白として描かれる。

- daemon: terminal ごとに client 単位の viewport（要求 geometry / 最後に渡した geometry）を保持し、
  `Resize` / `detach` / `disconnect` で再計算して PTY へ適用する。
- daemon: client が最後に受け取った geometry と現在の geometry がずれている間、その client の
  incremental poll（`Resume`）には `ResyncRequired` を返し、既存の再 attach 経路で screen を
  取り直させる。
- TUI: 要求 geometry（pane）と実効 geometry（daemon 権威）を分け、resize / attach の応答が返す
  実効 geometry を採用して local screen を組む。geometry fence は revision fence に一本化する。

wire の変更は不要（`Resize` の応答は既に snapshot に geometry を載せている）。
