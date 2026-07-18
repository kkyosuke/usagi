---
number: 275
title: fix(daemon): client build 世代に daemon lifecycle を自動追随させる
status: done
priority: high
labels: [daemon, ipc, lifecycle, cli, tui, mcp]
dependson: [341]
related: [268]
created_at: 2026-07-13T02:00:47.537577+00:00
updated_at: 2026-07-13T02:14:41.673199+00:00
---

## 目的

現行 client バイナリと接続先 daemon の build identity が一致する場合だけ endpoint を再利用する。debug build は production daemon と完全に隔離した development channel で毎回 lifecycle restart し、release/distributed build は production channel を安全に再利用する。手動 `usagi daemon restart` を恒常的に要求しない。

## スコープ

- build channel を data directory、socket locator、daemon record、lock、daemon-owned state まで隔離する。debug は development、release は production を使う。
- `USAGI_HOME` を明示した場合も同じ channel 分離を適用し、`cargo run --release` は production channel を選ぶ。
- IPC handshake の build identity を production bootstrap の世代判定に利用し、同一 build は既存 daemon を再利用する。
- debug bootstrap は同一 development channel の daemon を stop → 現在の binary で start → readiness と handshake を確認する。
- stale / build identity unknown / restart failure は、local fallback・blind retry をせず typed safe error として返す。
- 同時 client が lifecycle transition 中に二重 stop / spawn しないことを保証する。
- CLI、TUI、MCP、daemon subcommand の data directory 解決を統一する。

## 受け入れ条件

- debug と release の daemon endpoint、record、lock、state は同一 host/user で相互に参照・停止・置換しない。
- `cargo run` は development daemon を毎回一度だけ restart し、readiness と current build handshake の成功後だけ利用する。
- release/distributed binary は production channel の同 build daemon を child process を起動せず再利用し、旧 build は安全に rollover する。
- `USAGI_HOME` 明示時、`cargo run --release`、CLI/TUI/MCP/`daemon` 各 subcommand と同時 bootstrap の動作を test で固定する。
- 実装・test・daemon/IPC documentation を同じ PR に含める。
