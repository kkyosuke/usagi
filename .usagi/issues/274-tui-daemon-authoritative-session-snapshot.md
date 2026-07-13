---
number: 274
title: TUI を daemon authoritative session snapshot へ接続する
status: done
priority: high
labels: [bug, tui]
dependson: []
related: []
created_at: 2026-07-13T02:00:08.993835+00:00
updated_at: 2026-07-13T02:07:20.196786+00:00
---

## 背景

`src/runtime/tui.rs` の `DaemonSessionCommandPort` は daemon の `Accepted` メッセージだけを文字列化し、`launch_workspace` は legacy `WorkspaceStateStore` snapshot から Home を描画している。このため session create/remove の成功後に sidebar/overview が更新されず、失敗したように見える。

## 目的

TUI runtime を daemon lifecycle state を唯一の正本とする session snapshot/replay 経路へ接続し、create/remove 完了後に authoritative snapshot を即時反映する。

## 受け入れ条件

- 起動時・reconnect/reload・create/remove の final 後に daemon の workspace lifecycle snapshot を取り込み、sidebar/overview を更新する。
- 既存の `accepted/progress/final`、operation ID/revision、同名再作成、stale snapshot の fence を lifecycle reducer/adapter の語彙のまま利用する。
- daemon unavailable・wire/reconcile error は安全に表示し、pending state を不正に確定・巻き戻ししない。
- legacy `WorkspaceStateStore` へ session lifecycle を書き戻さず、TUI 側で local worktree を作成しない。
- runtime/composition 境界をテストし、必要な仕様ドキュメントを更新する。
