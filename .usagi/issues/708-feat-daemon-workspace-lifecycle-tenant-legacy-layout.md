---
number: 708
title: feat(daemon): workspace lifecycle 文書を tenant ごとに分離し legacy layout から移行する
status: todo
priority: high
labels: [v2, daemon, lifecycle, workspace]
dependson: []
related: []
created_at: 2026-08-20T23:33:23.460781+00:00
updated_at: 2026-08-21T00:09:55.259254+00:00
---

設計は [document/proposals/17-multi-workspace-daemon.md](../../document/proposals/17-multi-workspace-daemon.md) が正本。本 issue はその段階 1。

## 問題・根拠

`<data-dir>/daemon/sessions.json` は `repository_root` と root worktree id を **1 組しか持てない**文書であり、
1 つの daemon が 2 つ目の workspace の lifecycle を書く場所が無い。multi-tenant 化の前提として、この文書だけを
workspace ごとに分ける必要がある。

locator・`daemon.json`・単一インスタンス lock・generation registry・allocator・shard・dispatch・inbox・PR inventory は
**data directory 単位のまま**にする（daemon は machine あたり 1 つのままなので分ける理由が無い。socket path も
subtree を経由しないので `sun_path` の長さ予算に影響しない）。

## 方針

- `<data-dir>/daemon/w/<digest>/sessions.json` を tenant ごとの workspace lifecycle 文書にする。
- digest は bootstrap broker key と同じ作り（domain separation tag + length prefix + SHA-256、先頭 6 byte を hex 12 文字）。
- subtree に `root.json`（canonical workspace root）を置き、不一致なら suffix を伸ばして次の候補を見る（短縮 digest の衝突で別 workspace の state を書かない）。
- 置き場所の決定は単一の resolver に集約し、`SessionRuntime` の state store をその path で開く。
- legacy `<data-dir>/daemon/sessions.json` は初回起動時に `repository_root` の subtree へ rename で一方向移行し、`runtime-migration.json` と同じ形で記録する。

この段階では tenant は 1 つのままで、挙動を変えない。

## 受入条件

- digest の決定性・domain separation、`root.json` 不一致時の probing、resolver の path が unit test で固定される。
- legacy layout の fixture から起動すると `repository_root` の subtree へ着地し、legacy 位置に lifecycle 文書が残らない。
- 既存の daemon 結合テスト（起動・session 作成・restart をまたぐ state 共有）が挙動不変で通る。
- カバレッジ 100% を維持する。
