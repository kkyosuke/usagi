---
number: 709
title: feat(daemon): generation socket sweep を workspace subtree へ閉じ、2 workspace 同時稼働を成立させる
status: todo
priority: high
labels: [v2, daemon, lifecycle, workspace]
dependson: [708]
related: []
created_at: 2026-08-20T23:33:39.823990+00:00
updated_at: 2026-08-20T23:33:39.823990+00:00
---

設計は [document/proposals/17-multi-workspace-daemon.md](../../document/proposals/17-multi-workspace-daemon.md) が正本。本 issue はその段階 2。#708 の layout 分割が前提。

## 問題・根拠

起動時の `remove_recoverable_generation_sockets` は generation directory を走査し、自分の registry が preserve
しない entry の socket を residue として回収する。generation directory が data directory 単位のままだと、
**workspace A の daemon が workspace B の live socket を消す**。したがって layout を分けただけでは 2 daemon の同時稼働は成立しない。

## 方針

- sweep の走査範囲を `w/<digest>/g/` に閉じる。preserve 判定はその workspace の generation registry と 1 対 1 になるため、判定ロジック自体は変えない。
- 別 workspace の subtree へは読み書きしない invariant を、sweep・cleanup・retire のすべての経路で守る。
- bootstrap broker（既に workspace × executable の digest で分離）はそのままで、broker が起こす daemon の cwd が対象 workspace であることを確認する。

## 受入条件

- 2 つの fixture workspace で daemon を同時に起動し、それぞれが自分の session だけを serve する結合テストが通る。
- A の起動時 sweep のあとに B の socket と locator が生存していることを直接 assert する。
- 一方の daemon の stop / crash が、他方の locator・registry・shard を変更しない。
- daemon 起動は `tests/support/daemon.rs` の command builder 経由（cwd 隔離と exact reap）で行う。
- カバレッジ 100% を維持する。
