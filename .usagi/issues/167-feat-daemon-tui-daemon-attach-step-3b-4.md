---
number: 167
title: feat(daemon): TUI を daemon 端末の attach クライアント化（Step 3b-4）
status: done
priority: high
labels: [daemon, tui, ipc]
dependson: [166]
related: []
parent: 159
created_at: 2026-07-10T13:35:38.863387+00:00
updated_at: 2026-07-10T21:14:44.341278+00:00
---

Epic #159 の Step 3b の最終スライス。TUI の `TerminalPool` を **daemon 所有端末への attach** に置き換え、TUI を「daemon 端末のビューア／入力クライアント」にする。ここで **TUI を閉じても agent が走り続ける**が実運用で成立する。3b-3（#166・`Keys`/`Resize`）に依存。

## やること

- TUI 起動時に daemon を autospawn（不在なら起動）。
- ペインを開く操作を、`spawn`（無ければ）＋`attach` に置き換える。
- daemon から届く `Screen` バイト列を端末ウィジェットへ描画。
- キー入力・リサイズを `Keys`/`Resize` として daemon へ送る。
- detach（Ctrl-O 相当）はローカルの購読解除にし、端末プロセスは daemon に残す。
- 既存の `TerminalPool`／`pane.rs`（PTY 直接所有）を撤去または attach 経由に付け替え。

## 難所・進め方

- `pool.rs` / `pane.rs` / `home/mod.rs` はカバレッジ除外の大型 TUI 内部。ロジック（attach 状態・描画差分の適用・入力エンコード）は可能な限り純粋関数へ切り出してユニットテストし、socket I/O と描画は合成ルート／薄いオーケストレータに閉じる。
- **実端末での手動検証が必須**（`run` / `verify`）。ヘッドレスの自動テストだけでは UI の正しさを担保できないため、専用スライスとして段階的に進める。
- IPC クライアント側（接続・フレーム受信・`Screen` 適用）は純粋部分をテストできる。

## 依存

- 前提: #166（daemon 端末への入力 Keys/Resize）。
- 完了で「閉じても走り続ける」が TUI 経由で成立し、Step 4（通知調停・マルチクライアント）へ進める。

## 設計

[document/proposals/02-daemon.md](../../document/proposals/02-daemon.md) の Step 3b-4。
