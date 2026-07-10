---
number: 168
title: feat(daemon): 孤児 adopt・マルチクライアント・通知調停（Step 4）
status: done
priority: medium
labels: [daemon, orchestration]
dependson: [167]
related: []
parent: 159
created_at: 2026-07-10T13:36:03.038440+00:00
updated_at: 2026-07-10T23:00:46+00:00
---

Epic #159 の Step 4。daemon が端末を所有し TUI が attach クライアントになった状態（Step 3b-4 #167）の上に、堅牢性と多クライアント運用の仕上げを行う。

## やること

### 通知調停
- Step 2 で保留したデスクトップ通知（waiting/done 遷移）を **daemon から発火**する。
- ただし TUI が attach 中の端末は二重通知を避ける（attach テーブルを参照して、観測者がいる端末の done 通知は抑制／waiting は前面でなければ通知、など TUI 既存ロジックの調停ルールを daemon 側へ移す）。

### マルチクライアント
- 同一端末に複数 TUI が attach した場合の入力調停（全 attach から受理／表示は全員同期、tmux 既定挙動）。
- attach 時の全画面スナップショット送信は実装済み（3b-2）。resize の競合（クライアントごとに希望サイズが違う）の扱いを決める。

### 孤児 adopt
- daemon がクラッシュ（正常 stop でない）で終了した場合、`setsid` した端末プロセスが孤児として残りうる。再起動時に daemon がそれらを検出して再 adopt するため、端末の pid を**永続化**（`<data-dir>/daemon/terminals.json`）し、起動時に生存突き合わせ（`process_alive`）して registry を復元する。
- 正常 stop では全端末 kill（実装済み）。adopt は異常終了ケースの回収。

## 純粋部分（テスト対象）

- 通知調停ルール（attach 状態＋phase → 通知するか）は純粋関数化してユニットテスト。
- 孤児 adopt の突き合わせ（永続 pid ↔ 生存判定 → 復元する／捨てる）も純粋化。

## 依存

- 前提: #167（TUI attach クライアント化）。

## 設計

[document/proposals/02-daemon.md](../../document/proposals/02-daemon.md) の Step 4（設計上の難所 4・5）。
