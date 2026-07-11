---
number: 201
title: fix(daemon): attach ハンドシェイク中にバッファされた Screen スナップショットが描画されない
status: done
priority: high
labels: [bug, daemon, tui]
dependson: []
related: []
created_at: 2026-07-11T03:03:15.584795+00:00
updated_at: 2026-07-11T03:06:47.623350+00:00
---

## 症状

daemon 所有端末（agent / terminal ペイン）に attach しても、ペインが空白のまま表示されないことがある。agent プロセス自体は daemon 側で正常に起動・生存しているため、ユーザーからは「agent が起動しない」ように見える。TUI を再起動してセッションへ復帰した直後や、既存 agent タブへの移動時に発生し、同じセッションで再度 `agent` を実行すると daemon に別の agent が二重起動して孤児として残る。

## 原因

`DaemonTerminal` の attach ハンドシェイク（`await_reply`）は `Attached` を見つけた時点で返るが、daemon は `Attached` の直後に初期 `Screen` スナップショットを push するため、同じ read で両方が `FrameDecoder` にバッファされることがある。その decoder を引き継ぐ reader スレッドのループが「まず `stream.read()` でブロック → その後 frame を drain」という順序だったため、バッファ済みの `Screen` frame は**次の出力が socket に届くまで処理されない**。idle な agent は何も出力しないので、ペインは空白のまま固定される。

## 修正

- reader ループを「decoder にバッファ済みの frame を全て drain してから read でブロックする」順序に変更。
- drain 判断は純ロジック `usecase::daemon_attach::drain_buffered_frames`（`DrainOutcome`）として切り出し、ユニットテストを追加（handshake 残置 frame の畳み込み・partial frame・decode 失敗・framing エラー・orphaned sink）。
- `ScreenSink` に `orphaned()`（既定 false）を追加し、reader 停止判断を trait 経由に統一。

## 再現手順（修正前）

1. セッションで agent ペインを開き、detach して TUI を終了する（open-panes が保存される）。
2. daemon と端末プロセスを落とし、TUI を再起動する（restore がペインを daemon に spawn する）。
3. セッションに集中して agent タブへ移動すると、agent は生きているのにペインが空白のまま更新されない。
