---
number: 205
title: fix(daemon): cargo run の旧 daemon 再利用を防ぐ
status: done
priority: high
labels: [bug, daemon, tui]
dependson: []
related: [339]
created_at: 2026-07-11T09:00:49.686203+00:00
updated_at: 2026-07-11T09:57:37.629714+00:00
---

## 症状

`cargo run` で再ビルドした usagi から `agent` を起動しても、以前の debug build で起動した常駐 daemon が再利用され、agent が表示されない、または plain terminal のように見える。

## 原因

TUI 起動時の daemon 再利用判定は `daemon.json` の PID 生存だけを見る。Cargo が `target/debug/usagi` を再ビルドして実行ファイルを置き換えても、既存 daemon は旧 inode のコードを実行し続け、新しい TUI と旧 daemon の build / IPC 世代が混在する。

## やること

- daemon terminal IPC の接続時に実行バイナリ世代を照合する。
- build が一致しない daemon への terminal spawn / attach を拒否し、手動の新規ペインは既存の TUI-local PTY fallback を使う。
- 旧 daemon の保存済み terminal と queued prompt 自動起動は local fallback せず、Agent の二重起動を防ぐ。
- handshake の純粋ロジック、wire roundtrip、IPC integration test を追加する。
- daemon fallback の仕様を更新する。

## 完了条件

- 同じ build の TUI / daemon は従来どおり daemon 所有 terminal を使う。
- `cargo run` 再ビルド後に旧 daemon が残っていても、新しい agent は plain terminal 化せず起動できる。
- 既存 daemon 所有 agent を強制終了しない。
- fmt / clippy / test / coverage が通る。
