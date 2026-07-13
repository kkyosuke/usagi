---
number: 280
title: fix(tui): Welcome/Open/Recent 起動の Overview session command port を実 daemon port に接続する
status: done
priority: high
labels: [tui, daemon, session, ipc]
dependson: []
related: [268, 274]
created_at: 2026-07-13T03:10:08.009353+00:00
updated_at: 2026-07-13T10:41:07.449472+00:00
---

## 目的

Welcome→Open / Welcome の Recent / direct Workspace entry のどの起動経路でも、Overview の `session create/list/overview/remove` が同じ daemon-authoritative の実 port を通るようにする。

現状 `usagi_tui::presentation::run_with_settings` は Welcome/Open/Recent から Workspace へ遷移するとき `UnavailableSessionCommandPort` を hard-code して注入しており、direct entry（`launch_workspace`）だけが `DaemonSessionCommandPort` を注入している。このため daemon restart 後に `cargo run` して Welcome/Open/Recent から workspace を開き Overview で `session create <name>` を実行すると `session commands are unavailable` になる。

## スコープ

- `run_with_settings` に session command port の factory を注入できるようにし、Welcome→Open / Recent / direct entry のすべてで同じ実 port を通す。
- port は workspace 起動ごとに fresh に生成する（daemon revision state を workspace 間で持ち越さない）。
- TUI crate（`usagi-tui`）は runtime/daemon を知らない層境界を維持する。trait は presentation が定義し、合成ルートが daemon-backed factory を実装・注入する。
- test 用に unavailable / fake の port・factory 注入を保てる設計にする。
- `document/03-tui.md` の Overview 実行 port の記述を、全起動経路で同じ実 port を通ることが明確になるよう更新する。

## 受け入れ条件

- Welcome→Open、Welcome の Recent、direct Workspace entry のすべてで、Overview の `session create/list/overview/remove` が同じ daemon IPC request になる。
- session command port は workspace 起動ごとに fresh に生成され、`UnavailableSessionCommandPort` の hard-code は screen graph の workspace 遷移から除去される。
- fake / integration test で、Welcome→Open 経由の `session create` が port に届き、返された snapshot が sidebar に反映されることを固定する。
- `usagi-tui` が runtime/daemon crate へ依存しない層境界が保たれる。
- 実装・test・`document/03-tui.md` を同じ PR に含める。
