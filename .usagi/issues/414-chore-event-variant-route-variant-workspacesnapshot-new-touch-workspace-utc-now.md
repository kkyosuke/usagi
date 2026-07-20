---
number: 414
title: chore: 死にコード小粒の束（Event 未構築 variant / Route 単一 variant / WorkspaceSnapshot::new / touch_workspace / Utc::now 直呼び）
status: todo
priority: low
labels: [chore, review]
dependson: []
related: []
created_at: 2026-07-20T11:55:22.084707+00:00
updated_at: 2026-07-20T11:55:22.084707+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。独立の小粒な死にコード・設計残渣をエリア横断で束ねる。

## 根拠と問題（検証済み）

1. `crates/daemon/src/usecase/terminal.rs:80-91` — `Event` enum の `Output`（:82）と `ResyncRequired`（:89-91）は**構築箇所ゼロ**。`Event::Exited` は `TerminalRegistry::exited`（:420, 構築 :424）が返すが、本番呼び出し元 2 箇所（runtime.rs:329-331、generic_terminal.rs:259-261）は戻り値を捨てている。
2. `crates/tui/src/usecase/application/controller.rs:34` — `Route` enum が単一 variant（`Home(HomeMode)`）。
3. `crates/tui/src/usecase/application.rs:65` — `WorkspaceSnapshot::new` が `SessionId::new()`/`WorkspaceId::new()` で識別子をでっち上げる。呼び出しはテストのみ（presentation/mod.rs:3077, :3409、いずれも `#[cfg(test)]` 境界 :2876 以降）。本番は `with_runtime_ids`（:78）を使う。
4. `crates/core/src/infrastructure/store/workspace.rs:144` — `touch_workspace` はロックなしの load→mutate→save（read-modify-write）で、かつ呼び出しはテストのみ（:200/:208/:213）の死にコード。
5. `crates/core/src/domain/workspace/mod.rs:31-32` — `Workspace::new` が `Utc::now()` を直呼びするが、本番呼び出しゼロ（テストのみ）。domain の他の `Utc::now()` 直呼びも同様に棚卸しする。

## 改善案（要検討）

- (1) 未構築 variant を削除するか、Exited/ResyncRequired を実際にイベント配信へ接続する（replay 有界化 issue と関連）。
- (2) Route を削除するか variant が増える具体計画に紐づける。
- (3) `WorkspaceSnapshot::new` を `#[cfg(test)]` へ隔離。
- (4) `touch_workspace` を削除（残すならロックを通る mutate API 化）。
- (5) domain コンストラクタは now 注入へ統一（未使用なら削除）。

## 受け入れ条件

- [ ] 上記 5 点それぞれについて削除/隔離/接続のいずれかが実施されている。
- [ ] coverage 100% を維持する。
