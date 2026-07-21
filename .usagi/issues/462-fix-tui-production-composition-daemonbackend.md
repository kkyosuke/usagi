---
number: 462
title: fix(tui): production composition を DaemonBackend に一本化する
status: done
priority: high
labels: [review, v2, tui, daemon, architecture]
dependson: [406]
related: [296, 314, 315, 316, 317, 319, 379, 405]
parent: 453
created_at: 2026-07-20T12:06:20.366664+00:00
updated_at: 2026-07-21T14:38:05.487888+00:00
---

## 問題・影響

root/v2 の `crates/tui/src/usecase/application/daemon_backend.rs::DaemonBackend::dispatch` は controller Effect の本番 executor としてテストされているが、`src/runtime/tui.rs` の production composition は生成せず、`crates/tui/src/presentation/mod.rs::dispatch_controller_effect` で別実装する。通常起動では decision/PR/browser/notification が unavailable、`WorkspaceCommand`・notes・environment が no-op、terminal arguments が破棄され、create 成功 `OperationResult` も還流しない。

## 成立条件 / 再現フロー

direct workspace と Welcome/Open/Recent から Home を開き、controller の全 Effect を発火する。入口ごとに異なる port/stub が注入され、同じ reducer contract でも production outcome が変わる。decision の caller 認可そのものは #406 が所有する。

## 対象責務と非対象

production composition を `DaemonBackend` とその host/push adapter に一本化し、重複 executor と compatibility stub を利用または削除する。各 subsystem の domain 実装、#405 SupervisorRuntime、#406 の credential/outbox 修正は非対象。

## 受入条件

- [ ] direct と screen graph の全入口が同じ production backend factory と port set を使う。
- [ ] 全 `Effect` が exactly one の実 action または明示 error completion に対応し、no-op/fallback success を残さない。
- [ ] create 成功も token 対応 `OperationResult` を返し、terminal open の arguments、notes/environment、workspace command を欠落させない。
- [ ] decision、PR snapshot、browser、desktop notification が production adapter に接続される。
- [ ] 旧 executor/stub/重複 host は production から除去し、正本を 1 つにする。

## 必須回帰テスト

direct と Welcome/Open/Recent の production composition test を用意し、全 Effect の route/completion matrix、create 成功/失敗、terminal argv、decision/PR/browser/notification/notes/env/workspace command を daemon fake または実 IPC で固定する。

## docs / 移行影響

`document/03-tui.md` と `document/02-architecture.md` の production data flow を一本化後の構成に更新する。永続 migration はないが、従来 silent no-op だった操作は明示結果を返す。
